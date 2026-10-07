# Changelog

Notable changes to each release, newest first.
The same notes are on the [GitHub Releases](../../releases) page.

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
