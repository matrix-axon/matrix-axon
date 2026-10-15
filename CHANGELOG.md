# Changelog

Notable changes to each release, newest first.
The same notes are on the [GitHub Releases](../../releases) page.

## v0.1.10 - 2026-10-15

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* fix: update rustls to the patched 0.23 release by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/714
#### Features
* feat(api): expose the SDK's calculated room display name by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/718
#### Bug Fixes
* fix(web): run the license listing through the pnpm that is running the build by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/722
#### Documentation
* docs: record remote push for suspended mobile clients by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/652
* docs: add a Fedora and RHEL install block to the README by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/712
* docs(adr): add ADR 0118 for room display names from the SDK by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/717
#### Build and CI
* ci(smoke): allow matrix-oauth suites to be dispatched by hand by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/707
* build(windows): sign executables and the installer with Azure Artifact Signing by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/719


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.9...v0.1.10

## v0.1.9 - 2026-10-13

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed

The biggest update is that the web client is now packaged with the Axon server, so you only need to install one package to have a complete, usable stack. This provides similar funtionality to the Docker stack we've been supporting for several months for users who want to run Axon natively rather than in Docker.

#### Features
* feat(web): open user information from timeline senders by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/680
* feat(api): serve the web client from the server binary by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/681
* feat(packaging): tell the operator about the web client and how to expose it by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/685
#### Bug Fixes
* fix(tui): derive room unread from the server's notification_count by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/676
* fix: bound database operations and prevent status queries from starving the server by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/616
* fix(api): keep the bootstrap token on screen until the operator continues by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/686
#### Documentation
* docs(adr): add ADR 0117 for Matrix OAuth browser sign-in by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/692
#### Build and CI
* fix(build): pin the bundle build id to the commit by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/675
* build: put the web client in the released server binaries by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/682
#### Other Changes
* create embed-web workflow as placeholder for testing by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/683


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.8...v0.1.9

## v0.1.8 - 2026-10-10

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat: show local notifications for new messages by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/615
* feat(api): management token and bind routes (ADR 0109 step 4) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/640
* feat: badge the shell icon from the unread total (#637) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/644
* feat: cache authoritative room member counts by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/645
* feat(tui): render cached room metadata and member counts in /whereami by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/660
* refactor: name shell icon-badge support on Platform by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/648
#### Bug Fixes
* fix: reconcile cached room metadata redactions by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/636
* fix(web): fetch thread roots six at a time, not all at once by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/665
* fix(web): wrap long error banners and record failed requests by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/664
* fix: give the Windows taskbar badge an accessible name by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/651
#### Build and CI
* ci: cancel superseded lint-and-clippy runs on PRs by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/650
#### Other Changes
* add link to Apple App Store install by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/649


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.7...v0.1.8

## v0.1.7 - 2026-10-07

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* minor version bump to address security advisory on source-map-js by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/629
#### Features
* feat(web): linked sign-ins with Unlink in Settings (ADR 0109 step 3) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/617
* feat: expose typed cached room metadata by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/626


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.6...v0.1.7

## v0.1.6 - 2026-10-05

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* feat(api): management gate, step-up, and identity list/unbind (ADR 0109 step 2) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/594
#### Bug Fixes
* fix(search): sort all matches before pagination by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/599
* fix(web): request search ordering before pagination by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/600
* fix(tui): support server search ordering and initial sort choices by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/601
#### Build and CI
* fix(ci): release-plz skips the release PR job when main has moved on by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/597
* fix(ci): release-notes ignores the 404 body when no release is published yet by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/596
* build: take every bundle's version from the Cargo release version by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/595


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.5...v0.1.6
