# ADR 0119 — Windows builds are signed with Azure Artifact Signing

**Status:** Accepted.
Implemented in `scripts/sign-windows.ps1`, `.github/actions/windows-signing` and the `windows` jobs of `cross-build.yml` and `desktop-build.yml` alongside this record.

## Context

Every Windows executable the release pipeline ships is unsigned: `axon-server.exe` and `axon-tui.exe` (zipped by `cross-build.yml`), and the Tauri shell and its NSIS installer (`desktop-build.yml`).
An unsigned executable shows SmartScreen's "unknown publisher" interstitial, and unsigned Rust binaries are a routine Defender and third-party antivirus false positive.
ADR 0102 listed a Windows Authenticode certificate as new signing infrastructure and left it undone.

Two facts shape the answer.

**CAs no longer hand out exportable keys.**
Since 2023 a newly issued OV or EV code-signing key must live on a hardware token or HSM, so the `WINDOWS_CERTIFICATE` base64-PFX-in-a-secret pattern the macOS lane uses is not available for a new certificate.

**Artifact Signing holds the key and issues short-lived certificates.**
Microsoft's service (formerly Trusted Signing) keeps the key in its own HSMs and signs on request; certificates last three days, so a timestamp is what keeps a signature valid.
The signing identity is an Entra app registration that can be federated to GitHub's OIDC, so no long-lived credential exists to leak.

## Decision

### Sign with signtool and Microsoft's dlib

`signtool sign /dlib Azure.CodeSigning.Dlib.dll /dmdf metadata.json` with `/tr http://timestamp.acs.microsoft.com`.
Both packages come from NuGet (`Microsoft.Windows.SDK.BuildTools`, `Microsoft.ArtifactSigning.Client`), the vendor's documented path.
This is the one integration that works both on a directory of files (the server and TUI) and as Tauri's per-file `signCommand` (the installer), so there is one script for both lanes.
`azure/artifact-signing-action` was rejected because an action cannot be invoked by Tauri mid-bundle, and a third-party CLI wrapper was rejected because a signing pipeline should not add an unaudited dependency.

### Sign the executables, not the archives

A zip cannot carry a signature, so `axon-server.exe` and `axon-tui.exe` are signed after the build and before `Compress-Archive`.
The web-embed assertion runs first, against the unsigned file.
Tauri signs the shell executable, the uninstaller and the installer itself, so the binary inside the installer is signed before it is packed.

### By presence, like every other lane

Signing is switched on by repository **variables**: `ARTIFACT_SIGNING_ENDPOINT`, `ARTIFACT_SIGNING_ACCOUNT`, `ARTIFACT_SIGNING_PROFILE`, `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID`.
None is secret: they name the service and the app, and the OIDC federation is what grants access.
With the endpoint unset, `.github/actions/windows-signing` skips login and setup, `sign-windows.ps1 sign` reports "not configured" and exits 0, and the build is unsigned exactly as before.
The Tauri `signCommand` is injected as a `--config` overlay only after setup has run, never committed to `tauri.conf.json`, so local builds are untouched.

### Once configured, unsigned is an error

`sign` verifies every file with `Get-AuthenticodeSignature` and fails unless the status is `Valid` and a timestamp is present.
The desktop job verifies the finished installer too.
A signing outage therefore fails a tag build, rather than silently shipping unsigned binaries on a release that users expect to be signed.
Signing is retried three times first, because the service and timestamp server are remote.

### OIDC is bound to an environment

Entra federated credentials match a subject exactly and take no wildcards, so a tag pattern cannot be expressed.
Both Windows jobs run in the `release-signing` GitHub environment, whose subject is `repo:<owner>/<repo>:environment:release-signing`.
Those jobs only run on tag pushes and manual dispatch, so an environment adds no friction to pull requests.
Restricting the environment to tag refs, or adding required reviewers, is how a compromised branch is kept from signing code.

### Local builds sign the same way

`scripts/package-windows.ps1` builds and signs all three applications and writes the release zips and installer.
`sign-windows.ps1` configures itself from `ARTIFACT_SIGNING_*` when `SIGN_*` are not already set, caching the NuGet tools per user, so the one script serves CI, Tauri's `signCommand` and a developer's shell.
Credentials are whatever DefaultAzureCredential finds, normally `az login`.
`sign-windows.ps1 overlay` writes the Tauri `--config` overlay, so CI and the local script use one definition of the sign command.

## Consequences

- One-time setup outside the repo: an Entra app registration with a federated credential for the environment subject, the **Artifact Signing Certificate Profile Signer** role on the certificate profile, and the `release-signing` environment plus the six variables.
- SmartScreen reputation still accrues with download volume; a signature removes "unknown publisher" and the Defender heuristics, not the first-weeks warning entirely.
- The signature publisher is the validated identity on the certificate profile, and is user-visible.
- The dependency versions are the latest at build time; pin them in `sign-windows.ps1` if a release build ever breaks on a package update.
- The Microsoft Store account is a separate distribution channel (Microsoft signs Store submissions) and is not part of this decision.
