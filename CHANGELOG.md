# Changelog

Notable changes to each release, newest first.
The same notes are on the [GitHub Releases](https://github.com/matrix-axon/matrix-axon/releases) page.

## v0.1.31 - 2026-11-22

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(web): play video and audio from stream tickets (ADR 0139 phase 3) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1190
* feat(server): report and resolve room member identity changes by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1187
* feat(server): report changes to an account's own Matrix identity by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1193
* feat(web): play video and audio in the shells through an axon-media scheme (ADR 0139 phase 4) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1191
* feat(web): stream video and audio in the Linux shell through a loopback server by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1202
* fix(ios): skip sender previews for Matrix links by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1215
* feat(tui): show decryption reasons and identity changes by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1204
#### Bug Fixes
* fix: hide spoilers in web room-list previews by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1198
#### Performance
* feat(deploy): use BBR congestion control for the stack's front doors by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1195
#### Documentation
* docs(adr): ADR 0141 — prefer the self-hosted CI runners when one is free by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1183
#### Build and CI
* ci: compile the Tauri shell crate on macOS for pull requests by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1199
#### Other Changes
* test(api): keep a minted ticket out of a test's assertion message by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1180


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.30...v0.1.31

## v0.1.30 - 2026-11-19

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(sync): verify stored room history once against the homeserver (ADR 0140 step 2) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1175
* feat(server): stream large video and audio from the homeserver (ADR 0139 phase 1) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1157
* feat(server): stream tickets for video and audio (ADR 0139 phase 2) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1174
#### Bug Fixes
* fix(web): restore automatic forward paging after a timeline jump by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1135
* fix(web): mark a room or thread read once a notification tap shows its newest message by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1145
* fix(web): say when a linked event cannot be opened by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1147
* fix(desktop): keep macOS from App Napping a backgrounded shell by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1159
* fix(sync): fill the holes limited syncs leave in room history (ADR 0140 step 1) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1143
* fix(web): recover link previews from large HTML heads by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1160
#### Documentation
* docs(adr): ADR 0139 — stream large video and audio by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1105
* docs: trim derivable directory layout and crates table from AGENTS.md by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1136
#### Build and CI
* ci(ios): reduce ios-sharing flakes on shared macOS runners by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1156
#### Other Changes
* minor tweak to fix prettier on install.md by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1134
* feat(web): link previews in edit mode (ADR 0132 delivery item 6) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1117
* feat(server): read linked events for Matrix-link cards (ADR 0138) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1141
* feat(web): Matrix-link cards built by the recipient (ADR 0138) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1144
* feat(server): resolve creator privilege and own level on /power_levels (ADR 0137, #324) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1155


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.29...v0.1.30

## v0.1.29 - 2026-11-17

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* feat(server): record why a message could not be decrypted (utd_reason) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1094
* fix(sync): record MismatchedSender as its own sender_trust verdict by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1102
* temporarily ignore picomatch security advisory until new version is available by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1127
#### Features
* feat(tui): paginate sidebar space membership by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1082
* feat(web): composer link previews (ADR 0132 PR 3) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1070
* feat(web): add /collapse and /expand timeline commands by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1095
* feat(web): add a copy text action to messages by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1099
* feat(web): make Favs independent and persist room-list preferences by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1118
#### Bug Fixes
* fix(web): stop iOS text autosizing on rotation by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1087
* fix(sync): one slow room no longer shuts the homeserver gate by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1083
* fix(sync): re-check backfilled UTDs whose key arrived during the page by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1096
* fix(web): keep a redacted thread root visible when hiding redacted events by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1098
* fix(tui): report recovery outcomes and retain them in /status by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1100
* fix(web): keep the Rooms button visible beside a long Account select by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1114
* docs(adr): ADR 0140 — fill the holes that limited syncs leave in room history by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1126
#### Performance
* feat(api): batch room-list previews in one request (#278) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1060
* feat(web): read room-list previews in batches behind the open room (#278) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1092
#### Documentation
* docs(adr): ADR 0132 § 8 — editing a message with link previews by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1069
* docs(adr): ADR 0137 — complete member list and resolved member roles by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1086
* docs(adr): ADR 0138 — previews of Matrix links, built by the recipient by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1097
* break out client notes by operating system and add instructions for spell-check in Linux by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1106
#### Build and CI
* ci: skip the Rust gate steps on pull requests that touch no Rust inputs by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1064
* ci: run pre-push hook entries from CI by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1103
* ci(windows): show license, Axon icon and docs link in the NSIS installer by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1115
* ci(cross-build): build Linux release zips on ubuntu-22.04 (glibc 2.35 floor) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1113
#### Other Changes
* ci: build the Linux desktop app on ubuntu-22.04 for older glibc by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1104
* feat(server): link previews on an edit (ADR 0132 delivery item 5) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1116


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.28...v0.1.29

## v0.1.28 - 2026-11-12

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(web): Room Information shows typed metadata, homeserver aliases and member counts (ADR 0133 PR A) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1044
* feat(web): show this install's device-state ID in the Settings footer by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1051
* feat(web): Room Information pages a space's children, names unjoined ones and previews them (ADR 0133 PR B) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1055
* feat(web): build link previews on the device in the shell (ADR 0132) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1045
* feat(tui): refresh room information from detail notifications by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1061
#### Bug Fixes
* fix(web): handle unread space labels in demo recordings by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1053
* fix(web): keep the message action bar on one line with (edited) and long names by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1062
#### Performance
* feat(api): incremental room-list reads with a since cursor (ADR 0134) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1046
* feat(web): incremental room-list refreshes with a persisted cursor (ADR 0134) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1047
#### Documentation
* docs(adr): ADR 0136 — exclude non-cross-signed devices (MSC4153) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1063
#### Other Changes
* feat(smoke): demo corpus gains an unjoined space child and advertised aliases (ADR 0133) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1052


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.27...v0.1.28

## v0.1.27 - 2026-11-10

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* ignore rustsec-2026-0459 security vulnerability on libsqlite3-sys until upstream rust-sdk updates this dependency by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1026
#### Features
* feat(tui): apply live redactions to the open timeline by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1027
#### Performance
* feat(web): incremental device-state reads with a persisted cursor by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1023
#### Documentation
* docs(adr): composer link previews on every platform (ADR 0132) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1025
* docs(adr): web Room Information metadata and progressive discovery (ADR 0133) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1028
#### Build and CI
* build(ios): derive the expected URL scheme from tauri.conf.json by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1029


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.26...v0.1.27

## v0.1.26 - 2026-11-09

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(server): fill in a selected unjoined room from the homeserver's room summary (ADR 0128 step 4) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1003
* feat(tui): pick a device for SAS verification by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1004
#### Bug Fixes
* fix(shell): restore PDF previews in packaged apps by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1002
#### Performance
* fix(shell): ask for compressed responses in the shell transport by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/994
* fix(shell): keep a busy HTTP/2 connection alive on a slow link by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/999
* feat(api): incremental device-state reads with a since cursor by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1018
#### Build and CI
* build: print each distinct license text once in the THIRDPARTY notices by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/1006


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.25...v0.1.26

## v0.1.25 - 2026-11-08

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(server): discover unjoined space children from the homeserver hierarchy (ADR 0128 step 3) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/985
* feat(web): render a shared gallery caption once by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/987
* feat(tui): show pending invites in a /pending popup by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/986
* feat(ios): share photos with captions, sizing, and recovery by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/989
#### Build and CI
* ci(playwright): switch apt mirror after a stalled install attempt by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/988


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.24...v0.1.25

## v0.1.24 - 2026-11-06

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* bump vodozemac to moot RUSTSEC-2026-0445 by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/979
#### Features
* feat(server): acquire homeserver room details and confirm known absence (ADR 0126 step 2) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/953
* feat(tui): join a room with /join by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/978
* feat(web): badge preference on push registration and Android read-room clearing (ADR 0129) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/973
* feat(ios): add text and rich-link Share Extension by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/962
#### Build and CI
* api-docs: fall back to most recent release with demo videos by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/972


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.23...v0.1.24

## v0.1.23 - 2026-11-05

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(web): replace standard emoji shortcodes while composing by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/952
* feat(server): badge and room-read pushes when an unread count falls (ADR 0129) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/963
#### Bug Fixes
* fix(search): fall back to plain words when a query is not valid syntax by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/965
#### Documentation
* ci(docs): fail the Pages build when a site link does not resolve by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/954
* docs(adr): ADR 0129, clear badges and read notifications on suspended phones by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/961
#### Dependencies
* refactor(web): remove tauri-plugin-http and the legacy shell transport toggle (ADR 0127 step 3) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/957
#### Build and CI
* ci(docs): fail the push gate on Liquid syntax in markdown by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/955


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.22...v0.1.23

## v0.1.22 - 2026-11-03

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(tui): present a 504 from leave or forget as still pending by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/928
* feat(web): turn on spell checking in the Linux shell by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/942
#### Bug Fixes
* fix(sync): re-decrypt a UTD whose key arrived before it was stored by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/940
#### Dependencies
* update async-executor with minor version bump by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/943
#### Other Changes
* test(api): pin the 504 for a membership call that times out by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/927


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.21...v0.1.22

## v0.1.21 - 2026-11-02

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(web): insert standard and custom emoji from the composer by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/894
* feat(api): add a paged space/hierarchy read (ADR 0128 step 1) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/925
* feat(web): present a 504 from an invite reject as still pending by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/926
#### Documentation
* escape documentation so jekyl won't choke on &#123;&#123;&#125;&#125; Markdown text when generating site docs by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/924
#### Other Changes
* rescue lost ADR 0128 commit by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/922
* feat(web): route the shell's requests through the pooled transport (ADR 0127) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/918


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.20...v0.1.21

## v0.1.20 - 2026-11-01

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Security
* update indirect dependency minimatch to mitigate CVE-2026-118764 by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/902
#### Features
* feat(web): wait past axon's deadline on calls that have a longer one by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/900
* feat(web): present a 504 from leave or forget as still pending by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/901
* feat: retain inline emoji media references after pack changes by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/895
* feat(ios): add shared sessions and durable incoming-share recovery by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/898
#### Documentation
* docs(adr): one shared HTTP client in the native shell (ADR 0127) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/892
#### Build and CI
* build(release): add axon-web, a package directory covering clients/web by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/911
#### Other Changes
* feat(web): pooled HTTP transport plugin for the native shell (ADR 0127) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/896


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.19...v0.1.20

## v0.1.19 - 2026-10-29

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Features
* feat(web): render bundled link preview images by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/877
* feat(tui): refresh an open /whereami popup by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/881
* feat(web): open attachment previews from compact cards by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/878
* feat(server): request guest access and server ACL state and publish room_details.changed by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/882
#### Bug Fixes
* fix(server): keep the sender-trust overlay watcher alive across subscription failures by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/883
#### Other Changes
* feat(server): feed the gateway's timeouts and server errors to the homeserver gate (ADR 0122 step 2) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/880


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.18...v0.1.19

## v0.1.18 - 2026-10-27

<!-- Release notes generated using configuration in .github/release.yml at main -->

### What's Changed
#### Bug Fixes
* fix(server): report an upstream timeout as a 504 and split the receipt and typing timeouts by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/870
#### Documentation
* docs(adr): room detail acquisition, state coverage, and change notification (ADR 0126) by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/847
#### Build and CI
* ci(web): retry the Playwright system package install when the mirror stalls by @ajkessel in https://github.com/matrix-axon/matrix-axon/pull/869


**Full Changelog**: https://github.com/matrix-axon/matrix-axon/compare/v0.1.17...v0.1.18

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
