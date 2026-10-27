# Changelog

Notable changes to each release, newest first.
The same notes are on the [GitHub Releases](../../releases) page.

## v0.1.17 - 2026-10-27

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* build(deps): bump compression-codecs for zstd 0.14 and drop stale advisory ignores by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/852
#### Features
* feat(web): flag unread messages in other accounts on the account selector by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/846
* feat(tui): filter /whereami members from the keyboard by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/848
* feat(server): generate opt-in homeserver link previews by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/851
#### Bug Fixes
* fix(web): copy a held timestamp's message text reliably on iOS by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/849
* fix(web): write the clipboard natively in the Tauri shell by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/850
#### Other Changes
* feat(sync): gate axon's background requests per homeserver (ADR 0122) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/836
* fix(sync): identify Axon to MAS with a User-Agent and logo, and re-register on metadata change by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/838


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.16...v0.1.17

## v0.1.16 - 2026-10-25

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* minor version bump on ctutils to resolve security vulnerability RUSTSEC-2026-0385 and RUSTSEC-2026-0381 by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/814
* update zod and undici to address dependency alerts by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/830
#### Features
* feat(server): discover image packs and send standalone stickers by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/812
* feat(web): select custom emoji and send stickers from image packs by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/813
* feat(tui): list /whereami members from the server projection by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/835
* feat(server): send bundled link previews with protected thumbnails by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/833
#### Documentation
* docs(server): give every OpenAPI operation a short title by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/831
#### Build and CI
* feat(api): build the web client when embed-web finds it missing or stale by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/829
#### Other Changes
* Chore/update ctutils by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/828
* manual tweak broke prettier--quick fix to restore CI by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/839


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.15...v0.1.16

## v0.1.15 - 2026-10-22

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* bump dompurify to address GHSA-cg2g-xh64-v4q7 by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/807
#### Features
* feat(web): scope space rail to the selected account and badge unread counts by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/806
#### Bug Fixes
* fix: require a UTF8 database, and create or convert one in the Linux packages by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/804
#### Documentation
* docs: slim the README and move operator reference into docs/ by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/805
#### Build and CI
* test(smoke): add a browser sign-in lane to the Matrix OAuth harness by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/802
#### Other Changes
* ci: let Release-plz be dispatched by hand by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/809


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.14...v0.1.15

## v0.1.14 - 2026-10-22

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* fix(deps): bump vite to 8.3.4 for the launch-editor advisories by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/786
#### Features
* feat: route APNs and FCM wakes for new messages by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/791
* feat(web): Markdown formatting shortcuts in the composer by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/795
* feat(web): render custom reaction images and names by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/796
* feat(server): expose custom reaction shortcodes by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/797
#### Build and CI
* ci: silence the Postgres nightly's drop-if-exists notices by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/792
#### Other Changes
* stop tracking .envrc so it can be used for local env settings by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/793
* minor version bump on vitest to address code injection security hole by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/798


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.13...v0.1.14

## v0.1.13 - 2026-10-21

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* build(deps): bump webpki crates in the desktop shell for RUSTSEC-2026-0381/0382 by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/783
* build(deps): bump webpki crates for RUSTSEC-2026-0381/0382/0383 by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/782
#### Features
* feat(server): serve custom reaction images through the media proxy by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/774
#### Bug Fixes
* fix: keep menus and tooltips inside narrow panes by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/776
* fix(smoke): follow the TUI's DM display names in the demo tour and report failures usefully by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/775
#### Build and CI
* ci: run the Postgres-backed ignored tests nightly on main by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/778


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.12...v0.1.13

## v0.1.12 - 2026-10-20

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(api): sign in to OAuth-only homeservers through the browser with the device grant by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/730
* feat(web): sign in through the homeserver's browser page by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/731
* feat: register device push tokens by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/734
#### Bug Fixes
* fix(deploy): give the Postgres containers a 256 MB /dev/shm by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/768
* fix(store): serve room thread summaries from a room-scoped index-only scan by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/764
#### Documentation
* docs(adr): guard the homeserver against axon's background load (ADR 0122) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/750


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.11...v0.1.12

## v0.1.11 - 2026-10-19

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(tui): use server-calculated room display names by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/738
* feat(web): title unnamed rooms from the server's calculated display name by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/733
* feat(web): render bundled URL preview cards by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/740
#### Bug Fixes
* fix(ci): verify the axon.exe packed in the installer, not the one left on disk by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/736
* fix: prevent reply avatars from overlapping quoted text by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/739
* fix(web): re-send a read receipt the homeserver did not accept by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/751
* fix(tui): re-send a read receipt the homeserver did not accept by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/752
* fix(sync): back off and park a failing backfill room instead of re-paging it from the live end by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/746
#### Documentation
* docs(adr): add ADR 0120 for publishing to winget by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/737
#### Dependencies
* build(desktop): ignore the zbus 5 unmaintained advisories by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/748


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.10...v0.1.11

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
