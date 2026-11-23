# ADR 0141 — Prefer the self-hosted CI runners when one is free

**Status:** Accepted.
Supersedes nothing.
Changes which machine runs some CI jobs, and no workflow's triggers.

## Context

The org now has two self-hosted runners, one Linux and one macOS, both labelled `self-hosted`.
They are faster than GitHub's hosted runners, but they are machines in a home lab: either can be offline, and each runs one job at a time.
Long workflows should use them when they can, and must never wait on them.
There is no Windows runner, so Windows jobs stay on GitHub.

### GitHub has no "prefer, then fall back"

`runs-on` names labels, and a job waits until a runner with those labels takes it, for up to 24 hours.
There is no setting that says "this runner if it is free, otherwise a hosted one".
The choice therefore has to be made before the heavy job is queued, by something that can see the runners' state.

The runners are registered on the org, not the repo (`GET /repos/matrix-axon/matrix-axon/actions/runners` lists none).
Reading the org's runners (`GET /orgs/matrix-axon/actions/runners`, which reports each runner's `status`, `busy` and `labels`) needs org-admin rights or the org's "runners and runner groups" fine-grained permission.
`GITHUB_TOKEN` can be given neither.

The org is on GitHub's Free plan.
As far as we know, that plan offers only the Default runner group and cannot restrict a group to selected workflows.

### The two machines

Both machines were inspected on 2026-11-19.
Each runner runs as its own unprivileged local account.
Neither machine is dedicated to CI: both are also used for other work, and both run other services.
This ADR records only the facts that its decisions depend on.
The rest of what the inspection found, and the hardening that followed, is kept with the maintainers rather than in a public repository.

**Linux.**

- Its Ubuntu release is newer than `ubuntu-latest`'s, and its glibc is newer than the 2.35 floor (see "The glibc floor" below).
- Development work on the machine uses the same CPU and memory.
- Its Docker daemon also runs services unrelated to CI, so the runner account cannot be put in the `docker` group, which amounts to root on the host.
- Other services hold host ports 5432 and 8008.

**macOS.**

- An Intel Mac. macOS 26 is the last release for Intel Macs, and Xcode 27 will not install on it, so this machine cannot follow the hosted images past Xcode 26.
- Docker is not available to the runner account.

### This repository is public

Pull requests from forks run workflows from the fork's merge ref, with the fork's copy of every workflow file.
A fork can edit `runs-on` to name the self-hosted label directly, so nothing in a workflow file can keep a fork PR off a self-hosted runner.
Code from that fork would then run on a machine that persists between jobs.

The repo's fork-PR approval policy is `all_external_contributors`, so every fork-PR run waits for a maintainer to approve it.

### Which workflows are long

Wall-clock times of the successful runs among the last ~400 runs, as of 2026-11-19.
They include queue time and every job in the run, so they are rough.

| Workflow                | Runs | Average (min) | Longest (min) |
| ----------------------- | ---: | ------------: | ------------: |
| Cross-platform build    |    1 |            49 |            49 |
| Web e2e                 |   24 |            33 |           445 |
| Native Linux packages   |   21 |            23 |            26 |
| CodeQL (default setup)  |   34 |            21 |            41 |
| iOS sharing             |   15 |            21 |            41 |
| Publish images          |    1 |            18 |            18 |
| postgres-tests          |    1 |            17 |            17 |
| Android build           |    9 |            17 |            22 |
| Rust lint and test      |   31 |            15 |            27 |
| Embedded web client     |   29 |            13 |            27 |
| Desktop build           |    9 |            12 |            25 |
| smoke                   |   34 |            10 |            31 |
| Release-plz             |    8 |             9 |            21 |
| Third-party disclosure  |   10 |             6 |            10 |
| Web lint and test       |   29 |             5 |            15 |
| Dependency advisories   |   25 |             5 |            15 |
| Deploy API docs         |   26 |             4 |            17 |
| Release notes           |    9 |             4 |             8 |
| OpenAPI compatibility   |   24 |             3 |            14 |
| Migrations forward-only |   25 |             3 |            13 |
| Notify opened           |   13 |             1 |             9 |

Run-level times overstate what one job costs in workflows with several jobs.
The per-job times below come from the last 15 successful runs of each of these workflows, as of 2026-11-19.
They measure only the time each job ran, with no queue time.
Decision 4 uses them to choose which job holds the self-hosted slot.

| Workflow and event                 | Job                            | Runs | Average (min) | Longest (min) |
| ---------------------------------- | ------------------------------ | ---: | ------------: | ------------: |
| Web e2e, `pull_request`            | `web-e2e (firefox)`            |   11 |           7.8 |            11 |
|                                    | `web-e2e (webkit-desktop)`     |   11 |           7.0 |             9 |
|                                    | `web-e2e (chromium)`           |   11 |           6.2 |             9 |
| Web e2e, `push`                    | `web-e2e (webkit-desktop)`     |    4 |           8.8 |            11 |
|                                    | `web-e2e-iphone`               |    4 |           8.2 |             9 |
|                                    | `web-e2e (firefox)`            |    4 |           7.5 |             8 |
|                                    | `web-e2e (chromium)`           |    4 |           5.8 |             8 |
| smoke, `push`                      | `true-local`                   |    4 |          18.5 |            21 |
|                                    | `matrix-oauth` (called)        |    4 |          14.8 |            16 |
|                                    | `stub`                         |    4 |           1.8 |             3 |
| smoke, `pull_request`              | `stub`                         |   11 |           0.6 |             3 |
| Integration test, dispatch         | `matrix-oauth` (called)        |    8 |          10.6 |            14 |
|                                    | `integration`                  |    7 |           6.1 |             7 |

`gap-fill` had no successful run in that window.

### What already assumes a hosted runner

- **`.github/actions/install-playwright`** rewrites the runner's apt sources file in place and runs `playwright install-deps` under `sudo`.
  Its own comment says it assumes an ephemeral runner, because on a self-hosted one that rewrite would persist into later jobs.
- **Jobs that `sudo apt-get install`**: `desktop-build.yml` `checks` and `linux`, `cross-build.yml` `linux`, `package.yml`.
- **Cache keys** mostly use `runner.os`, which is `Linux` on both kinds of runner.
  A `target/` directory built against the self-hosted machine's system libraries would be restored on `ubuntu-latest`, and the other way round.
  `desktop-build.yml` already puts the runner image in its Linux key for exactly this reason.
- **Fixed host ports.**
  `embed-web.yml` and `postgres-tests.yml` publish their Postgres service on host port 5432.
  `scripts/lib/synapse-itest.sh` publishes Synapse on `localhost:8008` and uses `docker compose` without `-p`, so the project name comes from the checkout directory.
  Two jobs on one machine, or a Postgres the machine runs for itself, would collide.
- **The glibc floor.**
  `cross-build.yml` `linux`, `desktop-build.yml` `linux` and `package.yml` are pinned to `ubuntu-22.04` so their binaries link glibc 2.35, and `scripts/assert-glibc-floor.sh` fails the job otherwise.
- **Signing.**
  `apple-store-build.yml` and `desktop-build.yml` `macos` import Apple certificates into a keychain.
  `scripts/ci/store-credentials.sh cleanup` already deletes its keychain, "for the day someone points the workflow at a self-hosted one".
- **An earlier self-hosted runner.**
  `.github/actionlint.yaml` still declares the labels `binary-builder` and `api-builder`, and the `lint-and-clippy.yml` cache key still starts with `binary-builder-`.
  No workflow uses either label any more.

## Decision

### 1. Each long workflow picks its runner in a short job first

A new reusable workflow, `.github/workflows/pick-runner.yml` (`on: workflow_call`), runs one job on `ubuntu-latest` and outputs one `runs-on` value per pool, as JSON.
A caller adds a `pick` job and makes the heavy job depend on it:

```yaml
jobs:
  pick:
    uses: ./.github/workflows/pick-runner.yml
    secrets:
      RUNNER_PICKER_PRIVATE_KEY: # the expression secrets.RUNNER_PICKER_PRIVATE_KEY

  lint-and-test:
    needs: pick
    runs-on: # the expression fromJSON(needs.pick.outputs.linux)
```

Each comment stands in for the real expression, which this page cannot spell out: the site's Jekyll build would read the expression delimiters as Liquid (`scripts/check-liquid.sh`).

The caller passes the picker exactly one secret, by name, rather than `secrets: inherit`, which would hand it every repository secret.
The App's client ID is not a secret.
It lives in the repository variable `RUNNER_PICKER_CLIENT_ID`, which the called workflow reads directly.
(This ADR first named the variable `RUNNER_PICKER_APP_ID`. `create-github-app-token` v3 deprecates the App ID in favour of the client ID, so the pilot stores that instead.)

`outputs.linux` is either `["self-hosted","axon-linux"]` or the job's hosted Linux image.
`outputs.macos` is either `["self-hosted","axon-macos"]` or the job's hosted macOS image.
Callers pass both hosted fallbacks as inputs, so a job pinned to `macos-26` falls back to `macos-26`.
The Linux input defaults to `ubuntu-latest`, which every Linux job in decision 5 uses today.
The PR that moves a job sets the input to whatever the job's `runs-on` was, so moving a job never changes its hosted image.

The picker returns the self-hosted value only when all of these hold, checked in this order:

1. **The repository variable `SELF_HOSTED_RUNNERS` is not `off`.**
   This is the kill switch for maintenance or an incident, and it takes effect without a commit.
2. **The event is trusted.**
   Allowed events are `push`, `schedule`, `workflow_dispatch` and a `pull_request` whose head repository is this repository.
   Everything else falls back, including fork PRs and `pull_request_target`.
   So does any run whose `github.actor` is `dependabot[bot]`.
   GitHub withholds Actions secrets from those runs, so the picker could not authenticate anyway.
   Its PRs come from branches in this repository, so the head-repository test alone would let them through to check 3.
3. **The token secret is present.**
   Every event that reaches this check is one that GitHub gives secrets to, so a missing secret here means a misconfiguration (see decision 2's warning).
4. **More runners with the pool's label are idle than there are jobs already waiting for them.**
   - The picker counts the runners in `GET /orgs/matrix-axon/actions/runners` that carry the label and are `online` and not `busy`.
   - It counts the jobs in this repository that are queued for the label, by listing the queued and in-progress runs, then their queued jobs' `labels`.
   - The pool is free only when the first count is larger than the second.

   With one runner instance, that means the runner is idle and nothing is waiting for it.
   With several, the same rule applies, so a second instance (Open questions) needs no new picker logic.
   With one runner or several, it holds only as far as the waiting count is complete: that count covers this repository only, and Consequences accepts the gap that leaves.

Any API error, timeout or unexpected response falls back.
The step that makes the decision cannot fail the job: it writes the fallback output first and overwrites it only on a positive answer.
A failed picker would skip every job that `needs` it, which is worse than a slow run.

The logic is about 40 lines of `gh api` and `jq` in this repository.
Third-party actions that do this exist (for example `mikehardy/runner-fallback-action`), but adopting one means handing an outside action a token that can read the org's runners.

### 2. The token is a GitHub App's, scoped to reading runners

The App is installed on the org with read access to self-hosted runners (the "runners and runner groups" permission) and to this repository's Actions.
The picker mints a one-hour token on each run with `actions/create-github-app-token`.
The App's private key is the repository secret `RUNNER_PICKER_PRIVATE_KEY`, so fork PRs and Dependabot never see it.
The App's client ID is not secret and is the repository variable `RUNNER_PICKER_CLIENT_ID` (decision 1).
If the client ID is stored as a secret instead, the variable is empty, the token cannot be minted, and every job falls back to hosted runners.

We chose an App because, once it is set up, nobody has to maintain it.

- **Setup.**
  An App takes about ten minutes to create, install, and store as one secret and one variable.
  A fine-grained PAT with the same permissions takes about two.
- **Maintenance.**
  The App's key does not expire, and every run mints its own token.
  A fine-grained PAT expires, by default after at most a year, and has to be renewed by hand.
- **Ownership.**
  The App belongs to the org.
  A PAT belongs to one person, and stops working if that person's org access changes.
- **Rate limit.**
  The App has its own.
  A PAT shares its owner's rate limit with their everyday `gh` use.

What decided it is how the picker fails: when the token stops working, every job silently goes back to hosted runners (decision 1).
Nothing turns red, so an expired PAT would show up only as CI getting slower.
A fine-grained PAT stays the fallback if the App ever cannot be used.

Whatever the token, the picker emits a `::warning::` annotation when the runners API rejects it (401 or 403), or when the secret or the client ID variable is missing on an event that passed decision 1's check 2.
Dependabot runs never reach that check (decision 1, check 2), so they cannot raise a false warning.
A broken token then shows on every run's summary.
An offline or busy runner is the normal case and gets no warning.

### 3. Custom labels, and no fallback across operating systems

The runners get custom labels `axon-linux` and `axon-macos`.
The jobs target these rather than the default `self-hosted`/`Linux`/`macOS` labels, so a runner added later for some other purpose cannot pick them up by accident.
Both labels go in `.github/actionlint.yaml`, and the stale `binary-builder` and `api-builder` labels come out of it.

A Linux job falls back only to a hosted Linux image, and a macOS job only to a hosted macOS image.

### 4. One self-hosted slot per workflow run

A single picker output is shared by every job that reads it.
If several jobs or matrix lanes read it, they would all queue on one runner and finish one after another, later than on parallel hosted runners.
So in each workflow run, exactly one job, or one matrix lane, uses the pool's output.
Every other job in the run keeps its hosted `runs-on`.

**Reusable workflows count against the caller's slot.**
A reusable workflow that a long workflow calls (`matrix-oauth.yml`) does not run a picker of its own.
It takes its `runs-on` as an input whose default is its hosted image.
The caller passes the pool's output only when the called job is the one holding the slot.

**Who holds the slot.**
The holder is the longest job that runs in that run, from the per-job times in Context.
Where the longest job depends on the event, the holder does too:

| Workflow          | Slot holder                                                       | Why                                                                                                                        |
| ----------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `web-e2e.yml`     | `firefox` on pull requests; `webkit-desktop` on pushes; the one lane a single-project dispatch runs | The longest lane for each event. The lanes differ by under two minutes, and on pushes the hosted `web-e2e-iphone` job is only 0.6 min shorter, so the gain there is small. |
| `smoke.yml`       | `true-local`, on pushes and schedules                             | 18.5 min, against 14.8 for the called `matrix-oauth`, which stays hosted. `stub` takes under a minute and stays hosted.    |
| `integration.yml` | the one suite the dispatch selects; with `all`, `matrix-oauth`    | One dispatch normally runs one suite. With `all`, `matrix-oauth` (10.6 min) is longer than `integration` (6.1 min).         |
| all others        | their one moving job                                              | —                                                                                                                          |

For `web-e2e.yml`, the slot holder's name is this expression:

```text
github.event_name == 'push' && 'webkit-desktop'
  || github.event_name == 'workflow_dispatch' && github.event.inputs.projects != 'all' && github.event.inputs.projects
  || 'firefox'
```

Each lane's `runs-on` is then `matrix.project == <holder> && fromJSON(needs.pick.outputs.linux) || 'ubuntu-latest'`.
That gives:

- `pull_request`: `firefox`.
- `push`: `webkit-desktop`.
- A dispatch of `chromium`, `firefox` or `webkit-desktop`: that project's lane, which is the matrix's only lane.
- A dispatch of `all`: `firefox`.
- A dispatch of `webkit-iphone`: no matrix lane runs, and the `web-e2e-iphone` job stays hosted.

The `web-e2e` PR tests the expression for each of these events.
A dispatch project that matches no lane gets no slot and runs on hosted runners, so each lane writes the holder's name and its own runner to the run summary, which makes that case visible.
Before each workflow moves (decision 8), its holder is checked against current job timings, in case they have shifted.

### 5. Which jobs move

**Linux pool (`axon-linux`):**

Jobs marked "Docker" need the rootless daemon from decision 6, so they move only after the runner has it.

| Workflow                | Job                       | Notes                                                                                                                         |
| ----------------------- | ------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `lint-and-clippy.yml`   | `lint-and-test`           | On every PR and every push to `main`. Needs no Docker and no system packages. The pilot.                                      |
| `web-e2e.yml`           | one lane per run          | The slot holder from decision 4. Runs in the Playwright container image (decision 6), so the host's Ubuntu release does not matter. Docker. |
| `android-build.yml`     | `build`                   | Needs an Android SDK owned by the runner account; `sdkmanager` adds the pinned NDK to it.                                     |
| `embed-web.yml`         | `embed-web`               | Docker. Needs a dynamic service port, since the host's own Postgres holds 5432.                                               |
| `postgres-tests.yml`    | `postgres-tests`          | Scheduled. Docker, with a dynamic service port.                                                                               |
| `smoke.yml`             | `true-local`              | Pushes and schedules. Docker. `stub` stays hosted: it takes under a minute.                                                   |
| `matrix-oauth.yml`      | `matrix-oauth`            | Reusable. Gains a `runs-on` input (decision 4), which only `integration.yml` sets to the pool value. Docker; pulls from ghcr.  |
| `integration.yml`       | one of `integration`, `gap-fill`, `matrix-oauth` | Dispatch only; the slot holder from decision 4. Docker, with the compose isolation in decision 6, since services unrelated to CI hold 8008. |
| `desktop-build.yml`     | `checks`                  | Its apt packages are installed on the runner (decision 6). The machine's webkit2gtk is newer than the hosted image's; `checks` only compiles and lints against it. |

**macOS pool (`axon-macos`):**

| Workflow          | Job      | Notes                                                                                                                                         |
| ----------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `ios-sharing.yml` | `native` | Builds for `generic/platform=iOS`, so the Intel CPU does not matter. Its `brew install xcodegen` step is skipped on the self-hosted runner. |

**The macOS pool is temporary.**
The iMac cannot run Xcode 27 (see Context).
The pool is retired, by removing `macos` from `pick-runner.yml` and the label from the runner, as soon as any of these happens:

- a job in the pool needs an Xcode 27 SDK;
- the hosted `macos-latest` image defaults to Xcode 27;
- `store-credentials.sh xcode --min-sdk` is raised to 27.

Until then, `ios-sharing.yml` prints `xcodebuild -version` first on either kind of runner, so a difference between the two shows up in the log.

**Staying on hosted runners:**

- **Every Windows job.** There is no Windows runner.
- **The glibc-pinned jobs**: `cross-build.yml` `linux`, `desktop-build.yml` `linux` and `package.yml` `packages`.
  The Linux machine's glibc is newer than 2.35, so `assert-glibc-floor.sh` would fail them rather than fall back.
  They could join the pool only by building inside an Ubuntu 22.04 container, which is out of scope here.
  `package.yml` also builds on `ubuntu-22.04-arm`, and neither machine is ARM.
- **Jobs that sign or publish**: `apple-store-build.yml`, `desktop-build.yml` `macos`, `release-plz.yml`, `publish-images.yml`, and the `release` jobs of `cross-build.yml` and `package.yml`.
  Signing identities and registry logins should not pass through a machine that persists between jobs.
  Moving the macOS signing jobs is a separate decision.
- **Workflows that take under about six minutes**, from the table above.
  The picker's 10–15 seconds would be a large fraction of their time.
- **CodeQL.**
  It runs as default setup, which has no workflow file to edit.
  Default setup can use self-hosted runners labelled `code-scanning`, but then it always waits for them, with no fallback.

### 6. Jobs that move must not depend on an ephemeral machine

Each job that joins a pool gets these changes in the same PR that moves it:

- **Cache keys include the `runner.environment` context**, which is `github-hosted` or `self-hosted`, wherever the cached content depends on the machine (`target/`, pre-commit environments, Gradle).
- **System packages are installed on the runner ahead of time.**
  The steps that install them are skipped when `runner.environment == 'self-hosted'`, and replaced by a step that checks the packages are there and fails with a list of what is missing.
  This applies to `desktop-build.yml` `checks` and to `ios-sharing.yml`'s Homebrew step.
  The runner user gets no passwordless `sudo`.
- **The e2e lane runs in the Playwright container image** whose version matches `@playwright/test` in `clients/web/package.json`.
  The image already holds the browsers and their system libraries, so the lane needs neither `install-playwright`'s apt half nor a distro Playwright supports.
  The same image could later replace the apt step on hosted runners too, which would remove the mirror stalls `install-playwright` works around, but that is a separate change.
- **Docker for the runner is rootless.**
  The runner account gets its own rootless Docker daemon (`dockerd-rootless-setuptool.sh`, plus `loginctl enable-linger`), and is not added to the `docker` group.
  The host's daemon runs unrelated services, and access to it is access to root.
  A rootless daemon can see neither those containers nor the host's files.
  The job's `DOCKER_HOST` points at the runner account's socket through the runner's `.env`, which root writes when it sets up the runner (decision 7).
- **No fixed host ports.**
  On the Linux machine this is required, not just tidy: services unrelated to CI already hold 5432 and 8008.
  Service containers publish to a random host port (`ports: ["5432"]`) and the job reads `job.services.postgres.ports['5432']`.
  `synapse-itest.sh` and the smoke harnesses take their ports and compose project name from the environment, defaulting to today's values, so the scripts keep working unchanged on developer machines.
- **Docker state is torn down in an `if: always()` step**, by compose project name.
- **`check-environment` runs with `require-docker: true`** in every job that uses Docker, so a runner without it fails in seconds, not minutes in.
- **SDKs belong to the runner account.**
  The Android SDK is installed in the runner account's home, and root sets `ANDROID_HOME` in the runner's `.env`.
  No other account's SDK is shared with it, because sharing one would mean opening that account's home directory.

### 7. Fork PRs are kept off the runners at three levels

1. **The picker** routes fork PRs to hosted runners (decision 1, check 2).
   This keeps fork PRs working; it is not a security boundary, because a fork can edit the workflow.
2. **A job-started hook on each runner is the boundary.**
   The runner app runs the script named by `ACTIONS_RUNNER_HOOK_JOB_STARTED` before any step of every job, and fails the job if the script exits non-zero.
   The script lives on the machine, outside the repository, so a fork cannot change it.
   It reads `$GITHUB_EVENT_PATH` and allows only this repository and the events in decision 1's check 2, with the same head-repository test for `pull_request`.
   The script is kept in `scripts/ci/runner-job-started.sh` so it is reviewed like any other code, and is copied to each runner when it changes.
3. **Every fork-PR run needs approval** (`all_external_contributors`, set on 2026-11-19).
   Whoever approves a run must read its workflow diff.

Behind those three, the runner account is the last line of defence.
Before a runner takes jobs from this repository:

- **No administrator rights.**
  It has no `sudo`, and is not in `admin` (macOS) or in the `docker` group (Linux).
- **No other unprivileged account can write the runner's files.**
- **The runner account cannot write the files that enforce the hook.**
  Root owns the runner's install directory, and the runner account can only read it.
  That covers the runner binaries, the `.env` that names the hook, and the hook script.
  The runner account owns only `_work` and `_diag`.
  Otherwise any trusted job that runs hostile code, such as a `build.rs`, a postinstall script or a compromised dependency, could unset the hook variable or replace the runner's binaries, and later fork-PR jobs would pass unchecked.
  The pilot confirms the runner needs to write nothing else; anything it does need gets the narrowest grant that works.
- **The service refuses to start the runner if those files are writable by the runner account.**
  The check is a root-owned script that runs before the runner starts.
- **Root updates the runner.**
  The runner normally updates itself, which needs write access to its install directory.
  So it is configured with `--disableupdate`, and a maintainer installs each runner release as root, by hand, when CI shows a runner needs it.
  A root-owned timer can automate this later (a systemd timer on Linux, a LaunchDaemon on macOS) if manual updates become a chore.
- **It cannot read other accounts' home directories.**
- **It runs as a system service**, so the hook and `.env` survive a reboot.
  On Linux that is the root-owned systemd unit.
  On macOS it is a LaunchDaemon with `UserName` set, not the per-user LaunchAgent that the runner's `svc.sh` installs, because the runner account could edit that file.

**What these requirements do not stop.**
The runner needs to read its own registration credentials.
A job that already runs hostile code as the runner account can copy them and start its own runner, without the hook, under the same registration.
So the hook keeps fork code off a clean machine, but it cannot clean up a compromised account.
Getting to that point takes hostile code in a trusted job, which comes from code or a dependency that someone with write access merged, and that is the risk accepted below.
If a compromise is suspected, the runner is removed from the org, which revokes its credentials, and the account is rebuilt before the runner is registered again.

**Accepted risk: neither machine is dedicated to CI.**
A job can reach whatever listens on the host or its network.
Every such service needs credentials that CI jobs do not have.
The hook and the approval policy above mean only people with write access to this repository can put code on the machines.
We accept that for these machines.
A dedicated machine or VM comes first if either of these ever changes:

- someone outside that circle needs to run on them;
- CI needs credentials of its own there.

### 8. Rollout

1. **Prepare what the pilot needs:**
   - the runner-account requirements in decision 7, on both machines, including the root-owned install and its start-time check;
   - the custom labels;
   - allow this public repository in the Default runner group;
   - the job-started hook;
   - the GitHub App and its secrets.

   The pilot job needs nothing else installed: Rust and Python come from their setup actions.
2. **Pilot.**
   Add `pick-runner.yml`, the `runner-job-started.sh` source and the actionlint labels, and move only `lint-and-clippy.yml`.
   Run one runner instance on the Linux machine.
   Builds compete with development work for the same CPU and memory, so the pilot also records whether that hurts either side.
   Run it for about a week.
   For each run, record which pool the picker chose, why, the queue time, and the job time.
   The picker writes its choice and the reason to its step summary.
   The queue and job times come from the jobs API (`gh api .../actions/runs/<id>/jobs`), which also names the runner each job ran on.
3. **The remaining jobs in decision 5**, in a few PRs grouped by the decision 6 changes they need:
   - jobs that need no Docker: `ios-sharing`, `desktop-build` `checks`, and `android-build` once the runner account has its SDK;
   - jobs that need Docker, after the runner account's rootless daemon is in place.

   Before each of these PRs, re-read the moving workflow's per-job times from its recent runs (`gh api .../actions/runs/<id>/jobs`).
   Confirm or change its slot holder (decision 4) in that PR.
4. **Only if the pilot shows the need:**
   - **A sweeper.** A scheduled workflow that cancels any run whose job has waited for a self-hosted label longer than N minutes, then re-runs the whole run so the picker chooses again.
   - **Warm build state on the runners**, such as a local sccache, if restoring `actions/cache` over a home connection eats the gain.

## Consequences

- Long jobs run on the faster machines whenever those are free.
  When the machines are offline, busy or switched off, each job takes 10–15 seconds longer than today, which is the picker's cost.
- **Contention is reduced, not removed.**
  A push to `main` starts about eight of these workflows at once.
  Their pickers run within the same few seconds, and check 4 cannot see a job that another picker has not queued yet.
  Several jobs can therefore choose the same idle runner, and all but one wait for it.
  Check 4 also counts only this repository's queued jobs.
  The org's other repositories can use the Default runner group, and a job there that asks for the generic `self-hosted` label can land on these runners.
  The hook allows only this repository, so it fails such a job within seconds, before any step runs.
  The runner is busy only for those seconds, but a picker can still choose it just as one of those jobs takes it.
  We accept this rather than restricting the group, which the Free plan does not offer.
  Running two or three runner instances on the Linux machine makes this rare.
  Several instances on one machine are safe only when each also has its own account, or at least its own rootless Docker socket (Open question 1).
  Decision 6's port and compose changes are needed too, but they do not stop two instances' jobs from seeing each other's containers.
  Until open question 1 is settled, the Linux machine runs one instance.
  The sweeper bounds the wait if it is still common.
- **A runner that goes offline after the picker chose it** leaves the job queued until the runner returns.
  The job's `timeout-minutes` does not count queue time.
  Until the sweeper exists, the remedy is to cancel the run and re-run it.
- **Required status checks keep their names.**
  The new `pick` job is not one of them.
- **A second copy of the environment to maintain.**
  Packages, SDKs and Xcode on the runners must track what the hosted images provide for these jobs.
  The checks in decision 6 make drift fail loudly.
  The Linux machine runs a newer Ubuntu than `ubuntu-latest`, so a self-hosted pass is evidence for the newer distro and a hosted pass for the older one.
  The two are not interchangeable for anything that links system libraries.
- **CI and development share the Linux machine.**
  A cargo build from CI slows local builds there, and the other way round.
  The kill switch in decision 1 is also the remedy for a day of heavy local work.
- **The macOS pool will be retired** when the hosted images move to Xcode 27 (decision 5).
  Replacing it needs an Apple Silicon Mac, which is a separate decision.
- **One more secret, one more variable and one org App** to own.
  If any of them is missing or revoked, every job falls back to hosted runners, so nothing breaks, and decision 2's warning shows it.
- **Runner updates are ours.**
  With self-update disabled (decision 7), a maintainer installs each runner release by hand, and re-registers a runner whose registration has lapsed.
  GitHub stops sending jobs to a runner that falls 30 days behind a release, and the runners API does not report the runner's version.
  If an update is missed, the picker can keep choosing a runner that will never take the job.
  CI then shows jobs waiting on the self-hosted label, and the remedy is to set the kill switch, then update the runner.

## Alternatives considered

- **`runs-on: self-hosted` everywhere.**
  Fastest when the machines are up, but every job waits up to a day when they are not.
- **A fixed split: some workflows always self-hosted, the rest hosted.**
  Same failure when a machine is offline, and no use of a machine that is idle.
- **A `concurrency` group per runner.**
  A group holds one running and one pending run, and a newer pending run cancels the older one.
  That cancels CI rather than rerouting it.
- **Restricting the runner group to selected workflows.**
  Not available on the Free plan.
  Even where available, it matches the workflow file and ref, and same-repo PRs and fork PRs both run from `refs/pull/N/merge`.
- **Ephemeral runners (Actions Runner Controller, or just-in-time runners started per job).**
  These remove persistence, the risk behind decisions 6 and 7.
  They need an orchestrator we do not run, and they give up the warm caches that make the machines worth using.
  Worth revisiting if a fork PR ever needs to run on them.
- **A lock taken by creating a git ref** (creation fails if the ref exists), so only one picker can claim a runner.
  This would close the race in Consequences, but it needs write access to the repository's refs and a cleanup path for runs that die holding the lock.
  Not worth it until the pilot shows the race costs real time.

## Open questions

1. **A second runner instance on the Linux machine**, decided from the pilot's numbers.
   A second instance would also need a second account or a second rootless Docker socket, so its jobs cannot see each other's containers.
