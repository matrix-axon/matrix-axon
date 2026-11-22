# Self-hosted CI runners

How to set up and run the self-hosted runners that [ADR 0141](adr/0141-prefer-self-hosted-ci-runners.md) uses.
Long CI jobs use these runners when one is free, and fall back to GitHub's hosted runners otherwise.
The ADR explains why; this page is the procedure.

Only maintainers with org-admin rights can do these steps.
Commands that call `gh api orgs/...` need a token with the `admin:org` scope (`gh auth refresh -h github.com -s admin:org`).

## Once per org

### The runner picker App

`.github/workflows/pick-runner.yml` reads the org's runners with a GitHub App token, because `GITHUB_TOKEN` cannot read them.

1. Create the App: org settings, then **Developer settings**, then **GitHub Apps**, then **New GitHub App**.
   - Turn off **Webhook**.
   - Permissions: **Organization: Self-hosted runners: Read-only** and **Repository: Actions: Read-only**.
     Nothing else.
   - **Only on this account**.
2. Install it on the org, for this repository only.
3. Generate a private key, and store the key and the App's client ID:

   ```sh
   gh secret set RUNNER_PICKER_PRIVATE_KEY -R matrix-axon/matrix-axon < picker.private-key.pem
   gh variable set RUNNER_PICKER_CLIENT_ID -R matrix-axon/matrix-axon -b '<client ID, from the App page>'
   ```

   Then delete the downloaded key file.
   The client ID is a variable, not a secret: stored as a secret, the variable stays empty and every job falls back to hosted runners.

When either is missing or wrong, every job still runs, on hosted runners, and the `pick` job shows a warning annotation.

### The runner group

Org settings, then **Actions**, then **Runner groups**, then **Default**: turn on **Allow public repositories**.
Without it, this public repository's jobs cannot use the runners, yet the picker still sees them idle and sends jobs that then wait.

### The kill switch

```sh
gh variable set SELF_HOSTED_RUNNERS -R matrix-axon/matrix-axon -b off   # every job uses hosted runners
gh variable delete SELF_HOSTED_RUNNERS -R matrix-axon/matrix-axon       # back to preferring self-hosted
```

It takes effect on the next run, with no commit.
Use it for maintenance, for a day of heavy local work on a runner's machine, or when jobs are waiting on a runner that will not take them.

## Each Linux runner

The runner runs as its own unprivileged account (called `RUNNER` below), with no `sudo`, not in the `docker` group, and unable to read other accounts' home directories.
That account's primary group must hold no other account, because the runner's registration files are readable by that group.

The install is owned by root.
The runner account can write only `_work` and `_diag`, so no job can switch off the job-started hook for the jobs after it (ADR 0141, decision 7).
It therefore lives outside the runner account's home: whoever can write a parent directory can replace what is in it.

```sh
RUNNER=<runner account>
DIR=/opt/actions-runner
NAME=<runner name>
LIB=/usr/local/libexec/axon-runner
```

### 1. Retire an earlier registration

If the account already runs a runner from somewhere else, stop it, then remove that registration from its directory:

```sh
sudo -u "$RUNNER" ./config.sh remove \
  --token "$(gh api -X POST orgs/matrix-axon/actions/runners/remove-token --jq .token)"
```

Delete the old directory afterwards.

### 2. Unpack and register

`config.sh` refuses to run as root, so the runner account owns the tree until step 4.

```sh
VERSION=$(gh release view -R actions/runner --json tagName --jq .tagName | sed 's/^v//')
curl -fsSLo /tmp/actions-runner.tar.gz \
  "https://github.com/actions/runner/releases/download/v$VERSION/actions-runner-linux-x64-$VERSION.tar.gz"
sudo install -d -o "$RUNNER" -m 755 "$DIR"
sudo -u "$RUNNER" tar -xzf /tmp/actions-runner.tar.gz -C "$DIR"
cd "$DIR"
sudo -u "$RUNNER" ./config.sh --unattended --url https://github.com/matrix-axon \
  --token "$(gh api -X POST orgs/matrix-axon/actions/runners/registration-token --jq .token)" \
  --name "$NAME" --labels axon-linux --work _work --disableupdate
```

`--labels axon-linux` adds the pool label beside the default `self-hosted`, `Linux` and `X64`.
`--disableupdate` stops the runner updating itself, which would need write access to its own binaries (see "Updating the runner" below).

### 3. Install the hook and the start check

Both are root-owned copies of reviewed scripts in this repository.
Run this from a checkout of `main`:

```sh
sudo install -d -o root -g root -m 755 "$LIB"
sudo install -o root -g root -m 755 scripts/ci/runner-job-started.sh "$LIB/job-started.sh"
sudo install -o root -g root -m 755 scripts/ci/runner-preflight.sh "$LIB/preflight.sh"
echo "ACTIONS_RUNNER_HOOK_JOB_STARTED=$LIB/job-started.sh" | sudo tee -a "$DIR/.env"
```

When either script changes on `main`, install it again the same way.
The hook needs `jq`, and refuses every job if it is missing.

### 4. Make it a service, and give the install to root

```sh
sudo ./svc.sh install "$RUNNER"
sudo install -d "$DIR/_work" "$DIR/_diag"
sudo chown -R root:"$(id -gn "$RUNNER")" "$DIR"
sudo chmod -R g+rX,g-w,o-rwx "$DIR"
sudo chown -R "$RUNNER": "$DIR/_work" "$DIR/_diag"
```

Then make the service run the start check before every start.
The check fails, and the runner does not start, if any of these holds:

- the runner account can write the install, the hook, or any directory above them;
- a registration file (`.runner`, `.credentials*`) is readable by every account;
- the runner account cannot execute `runsvc.sh`, `bin/Runner.Listener` or `bin/Runner.Worker`.

It runs as the runner account, through the unit's `User=`, and refuses anything it cannot read.

```sh
UNIT=$(cat "$DIR/.service")
sudo install -d "/etc/systemd/system/$UNIT.d"
printf '[Service]\nExecStartPre=%s %s %s\n' "$LIB/preflight.sh" "$DIR" "$RUNNER" |
  sudo tee "/etc/systemd/system/$UNIT.d/preflight.conf"
sudo systemctl daemon-reload
sudo ./svc.sh start
```

### 5. Check it

```sh
sudo systemctl status "$UNIT"                 # active, and the preflight line says "not writable by ..."
sudo -u "$RUNNER" "$LIB/preflight.sh" "$DIR" "$RUNNER"   # the same check, by hand
sudo grep -rilE 'access to the path|permission denied' "$DIR/_diag" | head
```

The last command should print nothing.
If the runner logs a permission error for a file outside `_work` and `_diag`, give it the narrowest access that works, and note it on this page.

In any job that ran on the runner, the **Set up runner** step shows the hook's line: "Self-hosted runner: allowed push job from matrix-axon/matrix-axon."

### After changing permissions

The start check runs only when the service starts.
After any `chmod` or `chown` under the install, restart the service so it runs again:

```sh
sudo ./svc.sh stop && sudo ./svc.sh start
```

A runner that loses execute permission on its own programs while running keeps polling and accepts the next job, but cannot start the process that runs it.
The job then shows as in progress with no steps, and the runner's `_diag/Runner_*.log` shows it launching `Runner.Worker` with no "Process started" line after it.
Cancel the run, fix the permissions with step 4's commands, and restart.

### Updating the runner

Because the runner cannot update itself, a maintainer installs each runner release by hand.
GitHub stops sending jobs to a runner that falls 30 days behind a release, and the picker cannot see the runner's version.
The symptom is jobs waiting on `axon-linux` while the runner shows as idle: set the kill switch, then update.

```sh
sudo ./svc.sh stop
sudo tar -xzf /tmp/actions-runner.tar.gz -C "$DIR"      # the new release, downloaded as in step 2
sudo chown -R root:"$(id -gn "$RUNNER")" "$DIR"
sudo chmod -R g+rX,g-w,o-rwx "$DIR"
sudo chown -R "$RUNNER": "$DIR/_work" "$DIR/_diag"
sudo ./svc.sh start
```

If a registration lapses or is removed, register again from step 1.
If a compromise of the runner account is suspected, remove the runner from the org (which revokes its registration), rebuild the account, and register it again from step 1.

## The macOS runner

Not used by the pilot.
It is set up in the PR that moves `ios-sharing.yml`, with the same steps and these differences:

- The label is `axon-macos`, and the install lives under `/usr/local`, owned by `root:wheel` apart from `_work` and `_diag`.
- The service is a LaunchDaemon in `/Library/LaunchDaemons` with `UserName` set to the runner account, not the per-user LaunchAgent that `svc.sh install` writes, which the runner account could edit.
  Its program runs the start check, then `runsvc.sh`.
- `jq` is `/usr/bin/jq`.

## Checking the pilot

`lint-and-clippy.yml` is the only workflow that uses the picker.
To run it on a branch without opening a pull request:

```sh
gh workflow run lint-and-clippy.yml -R matrix-axon/matrix-axon --ref <branch>
```

This works because the workflow already exists on `main` with a `workflow_dispatch` trigger, and a dispatch runs the copy of the workflow on `--ref`.
A pull request from a branch in this repository also runs it.

Where each job ran, how long it waited, and how long it took:

```sh
gh api repos/matrix-axon/matrix-axon/actions/runs/<run id>/jobs \
  --jq '.jobs[] | {name, runner_name, labels, created_at, started_at, completed_at}'
```

The `pick` job's summary says which pool it chose and why.
