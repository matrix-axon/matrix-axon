#!/bin/sh
# Check postinst.sh's automatic UTF8 conversion (convert_axon_database and its
# use in configure) with su, systemctl and the catalog probes stubbed, so no
# Postgres or systemd is needed. Each stubbed command is logged, and a case
# checks which of them ran and what the admin was told.
set -e

root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/postinst-test.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
log=$scratch/commands
out=$scratch/output

# Sourced with no arguments, postinst only defines its functions.
# shellcheck source=packaging/scripts/postinst.sh
. "$root/packaging/scripts/postinst.sh"
# Its marker file goes here, so a case can check that none is left behind.
mkdir "$scratch/tmp"
TMPDIR=$scratch/tmp
export TMPDIR

# `su -s /bin/sh USER -c CMD`: CMD is $5.
su() {
	echo "$5" >>"$log"
	case $5 in
	createdb*) return "$CREATEDB_RC" ;;
	"pg_dump --encoding=UTF8 axon")
		echo "-- dump"
		return "$DUMP_RC"
		;;
	*"-d axon_utf8")
		cat >/dev/null
		return "$RESTORE_RC"
		;;
	*"ALTER DATABASE axon RENAME"*) return "$RENAME_RC" ;;
	esac
}
systemctl() {
	echo "systemctl $*" >>"$log"
	case $1 in
	stop) return "$STOP_RC" ;;
	show) echo "$STATE" ;;
	esac
}
is_systemd() { return "$SYSTEMD_RC"; }
pg_scalar() {
	case $1 in
	*pg_encoding_to_char*) echo "$ENCODING" ;;
	*pg_stat_activity*)
		[ -n "$SESSIONS" ] || return 1
		echo "$SESSIONS"
		;;
	"SELECT 1 FROM pg_database WHERE datname = '$EXISTING'") echo 1 ;;
	*pg_tablespace*) echo "$TABLESPACE" ;;
	*pg_database_size*) echo "$SIZE" ;;
	esac
}
cluster_free_bytes() { echo "$FREE"; }

# A SQL_ASCII database on a host where the conversion can go ahead.
reset() {
	case_name=$1
	: >"$log"
	: >"$out"
	CREATEDB_RC=0 DUMP_RC=0 RESTORE_RC=0 RENAME_RC=0
	SYSTEMD_RC=0 STOP_RC=0 STATE=inactive
	ENCODING=SQL_ASCII SESSIONS=0 EXISTING='' TABLESPACE=pg_default
	SIZE=$((100 * 1048576)) FREE=$((10 * 1073741824))
	axon_database_not_utf8
}

die() {
	echo "$case_name: $*" >&2
	echo "--- commands:" >&2
	cat "$log" >&2
	echo "--- output:" >&2
	cat "$out" >&2
	exit 1
}
convert() {
	if convert_axon_database RESUME >"$out" 2>&1; then rc=0; else rc=$?; fi
	[ "$rc" = "$1" ] || die "convert_axon_database returned $rc, expected $1"
}
ran() { grep -qF -- "$1" "$log" || die "expected to run: $1"; }
not_ran() { ! grep -qF -- "$1" "$log" || die "ran unexpectedly: $1"; }
said() { grep -qF -- "$1" "$out" || die "expected output: $1"; }
not_said() { ! grep -qF -- "$1" "$out" || die "printed unexpectedly: $1"; }
no_marker() { [ -z "$(ls -A "$TMPDIR")" ] || die "left a file in TMPDIR"; }
declined() {
	convert 1
	said "$1"
	said "  RESUME"
	not_ran createdb
}
failed_copy() {
	convert 1
	ran "dropdb --if-exists axon_utf8"
	said "failed (see the errors above)"
	said "is stopped now"
	said "  RESUME"
	no_marker
}

reset "converts"
convert 0
ran "systemctl stop axon-server.service"
ran "createdb -E UTF8 -T template0 --locale=C -O axon axon_utf8"
ran "ALTER DATABASE axon RENAME TO axon_old; ALTER DATABASE axon_utf8 RENAME TO axon"
not_ran dropdb
said "is now UTF8"
[ "$AXON_DB_ENCODING" = UTF8 ] || die "AXON_DB_ENCODING is $AXON_DB_ENCODING"
no_marker

reset "pg_dump fails"
DUMP_RC=1
failed_copy
not_ran "RENAME TO axon_old"

reset "restore fails"
RESTORE_RC=3
failed_copy
not_ran "RENAME TO axon_old"

reset "rename fails"
RENAME_RC=1
failed_copy

reset "createdb fails"
CREATEDB_RC=1
convert 1
not_ran pg_dump
said "creating 'axon_utf8' failed"
no_marker

reset "stop fails"
STOP_RC=1 STATE=active
declined "stopping axon-server failed"
not_said "is stopped now"

reset "unit still deactivating"
STATE=deactivating
declined "still deactivating after stopping it"
not_said "is stopped now"

reset "a client is connected"
SESSIONS=2
declined "client sessions (2) are still connected"
said "is stopped now"

reset "sessions cannot be counted"
SESSIONS=
declined "client sessions (count unknown)"

reset "no systemd"
SYSTEMD_RC=1
declined "without systemd"
not_ran systemctl
not_said "is stopped now"

reset "axon_old exists"
EXISTING=axon_old
declined "a database named 'axon_old' already exists"

reset "another tablespace"
TABLESPACE=fast
declined "not in the default tablespace"

reset "too little space"
FREE=$((1073741824))
declined "the copy needs about 1149 MB free"

# configure on an upgrade, with the rest of setup stubbed.
ensure_user() { :; }
ensure_dirs() { :; }
config_uses_local_socket() { return 0; }
local_postgres_ready() { return 0; }
store_key_check() { return 0; }
print_web_client_upgrade_notice() { :; }
CONFIG=$scratch/config.toml
: >"$CONFIG"

reset "upgrade that cannot convert leaves the unit stopped"
FREE=0
if configure >"$out" 2>&1; then rc=0; else rc=$?; fi
[ "$rc" = 0 ] || die "configure returned $rc"
said "is stopped now"
said "  sudo systemctl start axon-server"
not_ran "systemctl restart"
not_ran "systemctl start"

reset "upgrade that converts restarts the unit"
configure >"$out" 2>&1 || die "configure failed"
ran "RENAME TO axon_old"
ran "systemctl restart axon-server.service"

reset "upgrade on a UTF8 database does not stop the unit"
ENCODING=UTF8
configure >"$out" 2>&1 || die "configure failed"
not_ran "systemctl stop"
not_ran createdb
ran "systemctl restart axon-server.service"

echo "postinst conversion test ok"
