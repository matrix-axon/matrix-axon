# ADR 0120 — Publish axon-server, axon-tui and the desktop client to winget

**Status:** Proposed; tracked in [issue 728](https://github.com/matrix-axon/matrix-axon/issues/728).
Builds on ADR 0119 (Windows signing) and follows the Homebrew publishing job in `cross-build.yml`.

## Context

ADR 0119 signs the Windows executables and the Tauri installer on `v*` tags.
The release carries three Windows deliverables:

| Application   | Release asset                                                                     | Shape                                     |
| ------------- | --------------------------------------------------------------------------------- | ----------------------------------------- |
| axon-server   | `axon-server-windows.zip` (`axon-server.exe`, `THIRDPARTY`, `LICENSE.md`, flat)   | zip of one signed console executable      |
| axon-tui      | `axon-tui-windows.zip` (`axon-tui.exe`, `THIRDPARTY`, `LICENSE.md`, flat)         | zip of one signed console executable      |
| desktop shell | `Axon_<version>_x64-setup.exe`                                                    | signed Tauri NSIS installer, per-user     |

`axon-windows.zip` bundles the first two and is not packaged.
Everything is x64 only.

`winget` installs from a manifest in `microsoft/winget-pkgs` that points at a stable installer URL and carries its SHA-256.
Microsoft's validation pipeline downloads and scans the installer, which is why the signing in ADR 0119 comes first.
Two facts shape the design:

**A package cannot be created by an update tool.**
`wingetcreate update` and the `winget-releaser` action only bump a package that already exists, so the first version of each package is submitted by hand.

**The assets come from two workflows.**
`cross-build.yml` attaches the zips and `desktop-build.yml` attaches the installer, each in its own job, and a GitHub Release is created by whichever uploads first.
A `release: published` trigger therefore fires before the other workflow's assets exist.

## Decision

### Three packages, one per application

| Identifier              | Source                     | Installer type                          |
| ----------------------- | -------------------------- | --------------------------------------- |
| `MatrixAxon.AxonServer` | `axon-server-windows.zip`  | `zip`, nested `portable`, alias `axon-server` |
| `MatrixAxon.AxonTui`    | `axon-tui-windows.zip`     | `zip`, nested `portable`, alias `axon-tui`    |
| `MatrixAxon.Axon`       | `Axon_<version>_x64-setup.exe` | `nullsoft`, `/S`, `Scope: user`     |

The server is a portable package.
It registers no service and creates no configuration directory, which matches how the zip and the Homebrew formula already treat it.
A native installer for the server is a separate decision.

### Manifests are templates in the repository

`packaging/winget/<app>/*.yaml.tmpl` hold the version, installer and `en-US` locale manifests with `@@VERSION@@`, `@@TAG@@`, `@@SHA256@@`, `@@RELEASE_DATE@@` and `@@PUBLISHER@@` placeholders, in the style of `packaging/homebrew/*.tmpl`.
They are reviewed with the code, used for the one-time first submission, and give CI something to run `winget validate` against.

Steady-state releases do not render them.
`wingetcreate update` takes the previous manifest in `winget-pkgs` as the base, which keeps whatever the winget moderators adjusted, and only changes the version, URLs and hashes.

### Automate updates in a tag-driven job, not a release event

A job runs once per stable `v*` tag on `windows-latest`, after the three assets exist:

- It waits for the two zips and the installer the way `publish-tap.sh` waits for its assets (bounded retries on `gh release download`), rather than depending on a workflow the job cannot see.
- It computes nothing itself: `wingetcreate update <id> --version <v> --urls <asset-url> --submit` downloads the installers and hashes them.
- `alpha-*` and `beta-*` tags and a tag older than the version already in `winget-pkgs` are skipped, as in `publish-tap.sh`.
- It runs in a concurrency group of its own so two tags pushed together do not race, and takes a `workflow_dispatch` tag input to re-run a missed release.

Whether it lives in `cross-build.yml` beside the Homebrew job or in its own `winget.yml` is left to the implementing PR; it must not run before ADR 0119's signed installer is attached.

### Authentication

`wingetcreate --submit` opens a pull request from a fork of `microsoft/winget-pkgs`.
It needs a token that can push to that fork and open pull requests on `microsoft/winget-pkgs`, stored as a repository secret (`WINGET_TOKEN`).
Fine-grained tokens cannot target another organization's repository for pull requests, so this is a classic personal access token with `public_repo`, owned by whoever holds the fork.
That is a long-lived credential; it is scoped as narrowly as GitHub allows and rotated with the other release secrets.

### What this does not change

Signing, the release tag and the asset names are untouched.
Submitting to winget does not make a build "released": the GitHub Release remains the source of truth and winget follows it.

## Open decisions

1. **Publisher identity.** The certificate profile signs as `CN=Adam Kessel`, and winget shows `Publisher` to users.
   A winget package identifier is `Publisher.Package`; `MatrixAxon.*` is proposed, and `Publisher:` in the locale manifest must be chosen deliberately (`Matrix-Axon` or the signer's name), because it is user-visible and, for the installer, differs from the signing identity on the certificate.
2. **Fork owner.** Under `ajkessel` or under the `matrix-axon` organization.
3. **Server as portable only.** Confirmed by implication unless a service installer is wanted.
4. **First submissions.** Done by hand from a Windows machine after the first signed release, using the templates here.

## Consequences

- Winget users get `winget install MatrixAxon.Axon`, `MatrixAxon.AxonServer` and `MatrixAxon.AxonTui`, with SmartScreen reputation bootstrapped by the signature.
- Each stable release opens up to three pull requests on `microsoft/winget-pkgs`, merged after Microsoft's validation, so a version appears in winget hours to days after the GitHub Release.
- A failed validation (a flagged installer, a hash mismatch caused by re-uploading an asset) is a manual fix on the `winget-pkgs` side; replacing a release asset after the submission invalidates it, so assets are not replaced once a tag has published.
- The installer's `ProductCode` and the exact uninstall behavior are not in the templates: they must be read from a real install on Windows during the first submission.
