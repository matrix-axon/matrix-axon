#!/usr/bin/env bash
# Start-time check for a self-hosted runner (ADR 0141, decision 7).
#
#   runner-preflight.sh <runner-dir> <runner-user>
#
# Run it from the runner's service, before the runner starts: ExecStartPre= in the
# systemd unit, or the first command of the macOS LaunchDaemon's wrapper
# (docs/self-hosted-runners.md). It exits non-zero, so the service does not start
# the runner, if the runner account could disable the job-started hook. It needs
# no privileges: it only reads ownership and modes. Like the hook, the installed
# copy must be root-owned and outside the runner account's reach.
#
# It fails when any of these is owned by <runner-user>, or is writable by group or
# others:
#   - <runner-dir> and everything in it, except `_work` and `_diag`, which the
#     runner must write. That covers the binaries, `.env`, `.path` and the
#     registration files;
#   - the hook script named by ACTIONS_RUNNER_HOOK_JOB_STARTED in `.env`, which
#     must be set to an absolute path;
#   - every directory above either of them, because whoever can write a parent
#     can rename the child and put their own copy in its place.
#
# It also fails, because these break the runner or leak its registration, when:
#   - a registration file (`.runner*`, `.credentials*`) is readable by others:
#     its private key lets whoever reads it act as this runner;
#   - the runner account cannot execute `runsvc.sh`, `bin/Runner.Listener` or
#     `bin/Runner.Worker`, or search the directories that hold them. A runner
#     already running when that changes keeps polling and accepts a job, then
#     cannot start the worker, so the job hangs with no steps and no error.
# These are checked only at start: after changing permissions under <runner-dir>,
# restart the service so the check runs again.
#
# A parent directory may be writable by others if it is sticky (like /tmp) and
# not owned by <runner-user>: in a sticky directory only an entry's owner can
# rename it. Symbolic links are not checked themselves (their mode is always 777); what
# they point to is checked when it is inside one of the trees above.

set -uo pipefail

if (($# != 2)); then
  echo "usage: $0 <runner-dir> <runner-user>" >&2
  exit 2
fi
dir=${1%/}
user=$2
problems=0

problem() {
  echo "runner-preflight: $1" >&2
  problems=$((problems + 1))
}

id "$user" >/dev/null 2>&1 || {
  problem "no such user: $user"
  exit 1
}
[[ -d "$dir" && "$dir" == /* ]] || {
  problem "not an absolute path to a directory: $dir"
  exit 1
}

# Every scan fails closed: a path that find cannot read counts as a problem, so
# running this as an account that cannot see the tree refuses instead of passing.
# Run it as <runner-user> (the systemd unit's User=) or as root.
# scan leaves its matches in $found, not on stdout: a $(scan ...) would run in a
# subshell, and the problem it counts would be lost.
found=""
scan() { # what, find arguments...
  local what=$1
  shift
  if ! found=$(find "$@" 2>/dev/null); then
    problem "cannot read all of $what as $(id -un); run this check as $user or root"
  fi
}

# Prints the path if it fails the rule. -maxdepth 0 checks only the path itself.
bad() {
  find "$1" -maxdepth 0 ! -type l \( -user "$user" -o -perm -0020 -o -perm -0002 \) -print 2>/dev/null ||
    echo "$1"
}

# The same for a parent directory, which may be shared if it is sticky.
bad_parent() {
  find "$1" -maxdepth 0 \( -user "$user" -o \( ! -perm -1000 \( -perm -0020 -o -perm -0002 \) \) \) -print 2>/dev/null ||
    echo "$1"
}

check_ancestors() {
  local p=$1
  while [[ "$p" != / ]]; do
    p=$(dirname "$p")
    [[ -n "$(bad_parent "$p")" ]] && problem "$p (above $1) is owned by $user, or writable by group or others without the sticky bit"
  done
}

scan "$dir" "$dir" \( -path "$dir/_work" -o -path "$dir/_diag" \) -prune -o \
  ! -type l \( -user "$user" -o -perm -0020 -o -perm -0002 \) -print
while IFS= read -r p; do
  [[ -n "$p" ]] && problem "$p is owned by $user or writable by group or others"
done <<<"$found"
check_ancestors "$dir"

# Whether <runner-user> has the r or x permission on a path, through "others" or
# one of its groups. Owning the path is refused above, so the owner bits are not
# its to use.
user_groups=$(id -G "$user")
has_access() { # r|x path
  local gbit obit g
  case $1 in
    r) gbit=0040 obit=0004 ;;
    x) gbit=0010 obit=0001 ;;
  esac
  [[ -n "$(find "$2" -maxdepth 0 -perm -$obit -print 2>/dev/null)" ]] && return 0
  for g in $user_groups; do
    [[ -n "$(find "$2" -maxdepth 0 -gid "$g" -perm -$gbit -print 2>/dev/null)" ]] && return 0
  done
  return 1
}

scan "$dir" "$dir" -maxdepth 1 -type f \( -name '.runner*' -o -name '.credentials*' \) -perm -0004 -print
while IFS= read -r p; do
  [[ -n "$p" ]] && problem "$p is readable by every account; it holds the runner's registration"
done <<<"$found"

for p in runsvc.sh bin/Runner.Listener bin/Runner.Worker; do
  if [[ ! -f "$dir/$p" ]]; then
    problem "$dir/$p is missing, or this account cannot see it"
    continue
  fi
  has_access x "$dir/$p" || problem "$user cannot execute $dir/$p"
  d=$(dirname "$dir/$p")
  while :; do
    has_access x "$d" || problem "$user cannot enter $d, which holds $p"
    [[ "$d" == "$dir" ]] && break
    d=$(dirname "$d")
  done
done

hook=""
if [[ -f "$dir/.env" ]]; then
  hook=$(sed -n 's/^ACTIONS_RUNNER_HOOK_JOB_STARTED=//p' "$dir/.env" | tail -n1)
fi
if [[ -z "$hook" ]]; then
  problem "$dir/.env does not set ACTIONS_RUNNER_HOOK_JOB_STARTED"
elif [[ "$hook" != /* || ! -f "$hook" ]]; then
  problem "the job-started hook is not an absolute path to a file: $hook"
else
  [[ -n "$(bad "$hook")" ]] && problem "$hook is owned by $user or writable by group or others"
  check_ancestors "$hook"
fi

if ((problems > 0)); then
  echo "runner-preflight: refusing to start the runner ($problems problem(s)); see ADR 0141, decision 7" >&2
  exit 1
fi
echo "runner-preflight: $dir and its hook are not writable by $user, and $user can run it"
