# Share text and links to Axon on iOS

In Safari or another text/link app, choose **Share → Axon**.
If you have multiple active Matrix accounts, choose a sending account; a sole active account is selected automatically and its picker is hidden.
Use **Find room** to filter its destinations if needed, choose a room, review or edit the message, then explicitly choose **Send**.
Account and room choices open separate native list screens.
If no active Matrix accounts are available, the extension keeps your draft and asks you to open Axon → **Accounts** to add an account or sign in again, then reload.
An account without joined rooms instead asks you to join a room; logged-out accounts are excluded from sending choices.
The destination header includes a reload icon for refreshing accounts and rooms, with a spoken label for VoiceOver.
Routine introductory instructions are hidden; loading, errors, and delivery outcomes remain visible when relevant.
The composer remembers the last selected account and room on this device, including a selection made before cancelling a share.
Each new share restores that destination only after checking the current room list; an unavailable room leaves its account selected and asks you to choose another room.
Changing accounts clears the room selection, and signing out, changing servers, or removing the remembered account clears the saved preference.
Always review the displayed destination before choosing **Send**.
The room screen displays at most 100 matches; searching covers every loaded destination.
Axon does not send to the room currently open in the containing app.
The native extension works independently of the app's WebView; open Axon once and sign in before using it.
Text and web URLs are supported in this stage; photos, files, and mixed batches remain stage 7 of [the sharing plan](mobile-incoming-sharing.md).

The extension imports public URL/text representations without opening the linked website.
Text import prefers a bounded file representation and can fall back to Apple's public string and item readers when a source cannot supply a usable text file.
These paths enforce the 64 KiB UTF-8 staging ceiling; a file that exceeds the ceiling cannot bypass it through a fallback.
It folds duplicate representations of a URL together and preserves accompanying prose.
Source metadata is used only when actually supplied through a supported public input; a preview shown in the sender's share sheet does not establish that its metadata reached Axon.
No private metadata archives or property formats are read.
Compatible supplied metadata appears first without a lookup.
**Include preview** explicitly fetches missing metadata on the device through a restricted HTTPS reader, independently of account selection.
The website and its selected image host may receive that request; Safari and publisher-app cookies are not used.
The preview controls hide statically ineligible hosts, including IP literals and local/reserved names; DNS and every redirect/image address still undergo native validation before any connection.
The reader extracts Open Graph, Twitter Card, and ordinary HTML title/description fields without executing scripts.
**Use homeserver instead** is a separate action requiring an account and a confirmation that its Matrix homeserver may log the URL, including for encrypted rooms.
Its confirmation is anchored to the selected link's homeserver button.
Neither a device lookup failure nor a missing image automatically invokes that fallback.
The same order applies in the native share composer and the containing app's Pending shares recovery view.
Refreshing a card keeps its verified retained image if no usable replacement image is available; removing that image remains an explicit action.
Optional source titles that exceed the preview contract are dropped rather than making the original text unsendable.
Preview acquisition accepts an escaped ASCII URL subset supported by Foundation on iOS 15 and later; other links remain ordinary message text without an unusable preview action.
The **Link preview** section appears above the message editor and shows **Link only** beside **Include preview** until a preview is included.
At accessibility text sizes, the action moves below the status so its full label remains readable.
Information buttons beside **Link preview** and **Use homeserver instead** explain the corresponding network requests and privacy implications.
Viewing this information performs no lookup; homeserver lookup still requires separate confirmation.
**Send** stays in the navigation bar; after confirmed delivery it becomes **Done**.
Disabled or unavailable previews leave the original plain link editable and sendable.
Device status distinguishes unavailable metadata, a timeout, and a text card with an absent/unavailable image.
The separate homeserver action distinguishes disabled previews, an unsupported service, a timeout, an unavailable URL, a busy service, and missing usable metadata.
After generation, the status distinguishes an included image, metadata without an image, and an unavailable image.
An absent or unavailable image leaves a sendable text card; **Include preview** can be explicitly retried.
For messages with many links, **Find link for preview** searches all links while displaying at most eight choices at a time.
Cards support editing their title/description, removing their image, or removing the entire preview while retaining the URL.
**Edit preview** provides a multiline description editor that grows with the available display height and adjusts when the keyboard is shown.
The bulk **Remove previews** action appears only when more than one preview is included; both individual and bulk preview removal keep the links.
**Remove preview images** appears only when an included preview contains an image, including an unavailable retained image that needs removal.
Changing a URL invalidates its previous card.
Rendering cards never performs a webpage lookup or downloads an external image URL.

**Keep draft in Axon** saves local work without sending it.
Open Axon → **Pending shares** (also available in **Settings → Sharing** when the inbox is empty) to resume it under the [shared recovery policy](ios-sharing-foundation.md).
The inbox holds at most 20 pending shares, including failed imports retained from earlier attempts.
If it is full, send or explicitly discard existing drafts before sharing again; Axon preserves those drafts rather than dropping them to make room.
The extension reports the full inbox separately from oversized source text.
**Clear all** removes the confirmed set of pending shares; an individual **Discard share** removes one saved record.
Explicit discard can remove an idle draft immediately, including one still claimed by another composer; subsequent edits and sends from that composer are fenced out.
A share with a live dispatch remains protected until that request settles, so its delivery outcome is retained.
Discard during a thumbnail upload also fences the old composer before message dispatch; an upload already accepted by the server may remain there until its staging expiry.
Automatic expiry and quota cleanup still preserve actively owned shares.
Explicit discard can remove a saved record even if its local sharing-session descriptor is missing; sending, resuming, and acknowledging delivery still require the correct session.
Other deletion failures show their own error rather than being described as another open composer.
Text edits are saved after 400 milliseconds of idle time, and Keep draft/Send flush current edits.
An abrupt termination can lose edits still inside this batching window.
A debounce that fires during a blocking action is retried when the action settles while the draft is still owned.
If closing fails, lease renewal and editing remain available for retry; transient renewal errors preserve delivery-status messages.
**Cancel** interrupts provider reads and HTTP requests, stops queued work and heartbeats, and discards an undispatched share without sending it.
Cancellation reconciles the latest saved delivery state under the shared lock; an uncertain dispatch is retained without ownership for explicit recovery.
A message already dispatched may have been delivered; its saved outcome remains in Pending shares.
Cancel after confirmed success acknowledges and removes that completed record, as Done does; unknown delivery outcomes remain recoverable.
Confirmed success records the Matrix event ID.
An interrupted request or missing success response is **outcome unknown**, never automatically resent.
Inspect the destination before explicitly consenting to a possible duplicate in recovery.

The extension uses the app's eligible persisted access credential and never rotates OAuth refresh tokens.
Expired, inaccessible, or session-only credentials require opening Axon to refresh/sign in; an imported draft remains in Pending shares.
Before first launch, or while signed out, new staging is refused rather than saved under an invalid identity.
Authenticated Axon requests require HTTPS and retain default App Transport Security, certificate validation, bounded responses, and no redirects.
Room-list responses have an 8 MiB ceiling; thumbnail responses retain their separate 2 MiB ceiling.
An oversized response is reported with its limit and observed byte count, separately from connectivity errors.
Device preview acquisition admits one lookup per process with a shared 25-second page/image deadline and cancellable DNS waits of at most four seconds.
Every origin, redirect, and selected-image host must supply a validated public IPv4 A record.
Origin AAAA records are never connection targets: an apparently global IPv6 address can encode a private IPv4 service under an unknown operator NAT64 prefix.
A parallel, optional lookup of the fixed `ipv4only.arpa.` name discovers a canonical RFC 7050 NAT64 prefix under the same DNS budget.
Axon supports the RFC 6052 prefix lengths of 32, 40, 48, 56, 64, and 96 bits, but accepts only global-unicast or well-known NAT64 prefixes; local-use prefixes are unsupported.
Only Axon's own synthesis of the vetted public A address can authorize an IPv6 connection.
When that translation is available, its TLS connection starts first; the public IPv4 alternate starts after 250 milliseconds, or immediately if the first connection fails.
The first TLS-ready connection wins and cancels its competitor; only the winner sends an HTTP request, avoiding duplicate page or image requests.
IPv4 and supported NAT64 access networks can reach sites with public A records; IPv6-only origin sites cannot generate on-device previews.
Their links remain sendable, and the separate homeserver action remains an explicit alternative.
An absent, invalid, or timed-out discovery answer cannot authorize raw IPv6 or discard a usable public IPv4 target.
DNS requests enable authoritative negative answers, so a missing A record reports the public-IPv4 requirement and a negative discovery answer does not consume the full timeout.
DNS, connection racing, redirects, page reads, and image reads never reset the original deadline.
Every page, redirect, and selected-image request requires HTTPS and resolves to a validated global-unicast address; private, loopback, link-local, multicast, reserved, and documentation ranges are rejected.
The TLS socket connects to that numeric address while certificate and hostname validation still use the original DNS name, preventing a second unchecked DNS resolution.
There are at most six requests in each redirect chain, and no cookies, credentials, referrers, proxy fetches, JavaScript, frames, stylesheets, or arbitrary subresources are used.
Certificate validation disables auxiliary certificate-network fetches so attacker-controlled AIA/OCSP URLs cannot create unchecked requests; sites with incomplete certificate chains may therefore lack previews.
The transport accepts bounded HTTP/1.1 framing, requests identity encoding, caps headers at 32 KiB and chunks at 4,096, and stops HTML after the head or 256 KiB.
The inert SwiftSoup parser admits at most 512 potential tags before building its tree; unsupported or over-complex pages retain plain-link sending.
Selected images must complete within 2 MiB; ImageIO inspects dimensions against four million pixels before Axon decodes or converts them.
Generic public image formats such as TIFF/HEIC are converted to a bounded JPEG when needed for the existing upload contract.
Source-provided image reads retain their ten-second deadline and the same byte/pixel limits.
There is no fallback to Apple's network-managed `LPMetadataProvider`: supplied `LPLinkMetadata` remains a supported input, but preview acquisition is controlled by Axon.
The `device-preview` log category records DNS family availability, connection start/readiness/waiting/failure, numeric network error codes, response status/byte counts, and the stage of a timeout.
These diagnostics omit URLs, hostnames, IP addresses, headers, credentials, and page content.
Main-app preview acquisition uses one cancellable worker independently of the two upload/retention slots.
Closing recovery cancels and joins that work before releasing the draft, and late completions cannot restore a closed editor.

Import accepts at most 64 KiB of UTF-8 text; that is a staging ceiling, not a message allowance.
Send checks the shared canonical 32 KiB aggregate preview limit, UTF-16 field ceilings, and the separate serialized-event budget, including encrypted-image reservations and both protocol preview aliases.
Oversized drafts remain editable with Send disabled and a specific validation category.
Explicit server size rejection is a definite failure, never uncertain delivery.
Shorten text or remove excess cards; nothing is silently truncated.
Retained thumbnail bytes count toward protected staging quotas and are re-uploaded on each explicit Send, so expired server handles never trigger automatic webpage regeneration.
Missing or corrupt retained bytes require an explicit image removal or preview replacement.

## Building and checking

The committed sources are `clients/web/src-tauri/share-extension` and the common `incoming-share` Swift package.
After every manual `tauri ios init`, run `node scripts/configure-ios-sharing.mjs` from `clients/web`.
`scripts/package-ios.sh` runs the same hook after clean regeneration and always embeds the production `axon_share.appex`.
The separate `AXON_SHARE_PROBE=1` development probe remains optional and excluded from distribution exports.
Both the app ID `org.matrixaxon.axon` and extension ID `org.matrixaxon.axon.Share` need App Group `group.org.matrixaxon.axon.sharing` and matching shared Keychain access in their provisioning profiles.
Their marketing/build versions must match.
When automatic signing cannot select installed profiles, set `AXON_IOS_APP_DEVELOPMENT_PROFILE` and `AXON_IOS_SHARE_DEVELOPMENT_PROFILE` to manually managed profile names or UUIDs before running the generation hook or packaging script.
For `debugging`, the archive and export both use `Apple Development` and those development profiles.
For `app-store-connect` and `release-testing`, the archive and export both use `Apple Distribution` and the selected export profiles, matching the production push entitlement before export begins.
Explicit signing also generates Tauri's `ExportOptions.plist` with profiles for both bundles, because its automatic export does not select API-created profiles reliably.
Debug exports use those development profiles; App Store exports default to `iOS Team Store Provisioning Profile: <bundle ID>` names.
Override custom distribution profiles with `AXON_IOS_APP_EXPORT_PROFILE` and `AXON_IOS_SHARE_EXPORT_PROFILE`; debugging always uses the development profiles even when distribution overrides are present.
Manual hook invocations can select the export method with `AXON_IOS_EXPORT_METHOD`.
Manual exports require manually managed profiles for both bundles; Xcode rejects a managed containing-app profile in a manual export.
The Apple Store workflow selects both targets' archive and export profiles explicitly by validated UUID.
Its four iOS profile secrets must contain manually managed profiles with the required shared capabilities; the credential check rejects Xcode-managed profiles before building.
The Apple Store workflow also requires `IOS_SHARE_DEVELOPMENT_PROFILE` and `IOS_SHARE_APPSTORE_PROFILE` repository secrets for that extension ID; the credential check fails early when either is absent.
The exported-artifact checker verifies signatures, capabilities against embedded profiles, versions, default ATS, and text/web-URL-only activation rules.
Activation uses Apple's version 2 dictionary with strict text and single-web-URL matching.
The policy in `clients/web/scripts/ios-share-activation.json` is shared by generation, artifact checks, and simulator share-sheet tests.
UIKit can filter out a URL provider that also registers a full PNG representation; this is a media representation, not thumbnail metadata.
Standalone images and files remain outside this stage.
When the OS offers Axon for a mixed batch, intake rejects unsupported attachments and prevents sending a partial import.

```sh
pnpm --dir clients/web check:api
pnpm --dir clients/web test:sharing:native
pnpm --dir clients/web test:sharing:simulator
pnpm --dir clients/web lint
pnpm --dir clients/web test
pnpm --dir clients/web build
scripts/package-ios.sh --export-method debugging --install --device <device-identifier>
```

The simulator lane compiles the actual extension UI with extension-safe APIs and runs the shared native XCTest suite.
It also installs a disposable source app and the real extension controller under unique test identities, without shared credentials or App Group access.
UI tests open the system share sheet for text, URLs, sender-decorated links, standalone images, and URL/PNG alternatives, then check destination availability and cancellation from the real composer.
The simulator lane also exercises the production composer against a private staging root and stubbed HTTP.
It covers a stalled request, repeated cancellations, and loss of Send authority after another window discards the draft.
Native contract tests consume the existing server fixtures checked against OpenAPI; sender tests inspect durable intent before dispatch, confirmed event IDs, stale destinations, rejected authentication, explicit size rejection, missing thumbnail copies, response loss, and refusal to replay unknown operations.
These tests do not establish signed-device provider behavior or accessibility.
Diagnostics record share IDs, states, counts, and known representation flags; content, filenames, source URLs, credentials, and arbitrary provider errors are excluded.

## Signed-device acceptance

All live sends must go to **Axon Testing** (`!SScJmZuEkBUnuydXdf:bostoncoop.net`).
Use disposable local fixtures for other failure paths rather than sending to real rooms.

1. From Safari and another text/link source, share with Axon cold, warm, and suspended.
   Verify account/room choice, substantive accompanying text, no duplicated URL, and cancellation without a send.
2. Record actual public provider representations, including a sender that decorates its share sheet with `LPLinkMetadata`.
   Distinguish metadata received by Axon from metadata visible only in the sender.
3. Test source-supplied metadata and image fallback, URL-only explicit generation, disabled generation, hostile metadata, card editing/removal, and URL invalidation.
   Verify no lookup on arrival or render.
4. Send a generated image card to Axon Testing and inspect the delivered card in the recipient timeline.
   Verify encrypted delivery as well as an unencrypted fixture room.
5. Exercise expired/session-only credentials, offline server, stale destinations, request timeout, response loss, process termination, retained local thumbnails after handle expiry, and explicit removal of a missing thumbnail.
   Verify the recovery state before any retry.
6. Exercise Unicode and JSON escaping near aggregate-preview and encrypted/unencrypted event budgets.
   Oversized content must remain editable and definite size failures must stay distinct from unknown delivery.
7. Check VoiceOver, large Dynamic Type, keyboard access, iPad layout, and the clean exported IPA's extension/signing checks.

Record device measurements and remaining limitations here before treating issue [818](https://github.com/matrix-axon/matrix-axon/issues/818) as accepted.

## Verification recorded on 2026-11-01

The native suite passed 58 tests on macOS and 74 on the iOS simulator, including actual system URL/string providers, supplied title/image alternatives, separate-image rejection, image fallback/status reporting, protected thumbnail re-upload, response loss, cancellation, original-owner fencing, and a failed completion-publication checkpoint.
After rebasing onto upstream `main` at `7c3a9520`, both suites passed again, along with the two system share-sheet UI tests on the iOS 26.5 simulator.
The UI tests opened the real composer for plain text, a URL, and a sender-decorated URL, verified Send was disabled without credentials, and cancelled successfully.
The no-credential case also verifies that the status explicitly reports no saved draft and that **Keep draft in Axon** is absent.
They also confirmed that standalone images and URL/PNG alternatives did not advertise Axon as a media destination.
Transport tests exercise actual URLSession delegate callbacks, distinguish response-limit cancellation from network errors, and decode a 3.7 MiB room response containing 150 rooms with large topic fields.
The shared server/OpenAPI fixture and drift checks passed.
The rebased web suite passed 2,910 tests (two skipped).
Chromium end-to-end checks passed 253 tests (12 skipped) after rebasing, including the production build.
Lint and formatting passed after rebasing.
The signing-credential script suite and shell syntax checks passed.

Initial production packaging was blocked by the missing `org.matrixaxon.axon.Share` identifier/profile and unavailable Xcode automatic-provisioning authentication.
The identifier is now registered with the shared App Group, and manual development/App Store profiles were generated for both containing app and extension.
The installed containing-app App Store profile predated the sharing capability, so new profiles were needed for both bundles.
Production-identifier development and App Store exports passed the signed-artifact checker, including deep signature verification, profile capabilities, matching versions, default ATS, text/URL activation, and absence of the probe.
Clean `scripts/package-ios.sh` runs also passed for both debugging and App Store export methods, using the explicit manually managed profiles.
Both rebuilt release binaries from regenerated projects, exported the IPA, and passed the signing/activation, URL scheme, Apple sign-in, microphone/camera purpose-string, and push-environment checks.
The App Store export has the production push entitlement.
After rebasing, clean development and App Store exports of build 818.3 passed these checks again.
The GitHub repository still lacks `IOS_SHARE_DEVELOPMENT_PROFILE` and `IOS_SHARE_APPSTORE_PROFILE`; configuring CI provisioning remains a prerequisite before merge.
For device investigation only, a generated project temporarily used the existing `org.matrixaxon.axon.ShareProbe` development identity with the actual new extension controller and the display name **Axon share test**.
That signed development IPA exported and installed on both paired devices.
Repeated clean generation checks reproduced the production configuration; the device investigation overrides only the ignored generated project.
Device input measurements using this test identity remain separate from the production-identifier artifact checks above.
A separate `org.matrixaxon.axon.ShareUITests` development identifier/profile was registered for native UI automation, without sharing credentials or App Group access.
No login was reset.

The signed Safari → native composer → Cancel path subsequently passed on both iPhone and iPad, without dispatching a message.
Safari's page title and URL reached the native editor as editable text.
At 08:34:27 on the iPad, MobileSafari and SpringBoard both recorded `EXC_BAD_ACCESS`/`SIGSEGV` in `__66-[XCTAutomationSession initWithAccessibilityFramework:dataSource:]_block_invoke`.
No Axon app or share-extension crash report was present after pending reports were flushed.
Further XCTest UI runs on physical devices were stopped; these automation failures must not be treated as application acceptance or worked around in production code.
The earlier 08:25 jetsam report killed `FamilyControlsAgent` for its per-process limit; Axon was suspended and the extension active in that report, without a kill reason.

Manual iPhone testing exposed a room response exceeding the original 2 MiB ceiling (observed 2,098,282 bytes before cancellation).
The adapter now reports that cancellation honestly; room lists have a separate 8 MiB budget, and pre-send revalidation requests only the selected account.
Accounts then loaded, but the user reported the extension closing after account selection.
The updated development build replaces popup destination menus with separate list screens and bounded room rows.
The user confirmed that the updated iPhone and iPad builds stayed open, shared Safari URLs, and posted them to Axon Testing with recipient text preview cards.
The user subsequently confirmed that explicitly choosing **Include preview** produced the expected image; earlier text-only results reflected an undiscovered preview action.
The composer now places preview controls above the editor, shows its link-only state explicitly, and keeps Send/Done in the navigation bar.
The development build now reports the API's absent/unavailable/ready thumbnail outcome separately, and tests verify that both text-card fallback states remain sendable without a thumbnail fetch.
The signed fixture's URL-with-PNG-alternative action initially hid Axon, while its text/URL and sender-decoration actions offered Axon.
Neither the version 1 nor version 2 activation dictionary offered the URL-plus-PNG fixture on the signed devices.
The user confirmed that standalone-image and separate URL-plus-image inputs did not offer Axon.
The same absence was confirmed after installing the production-identifier build and restarting the fixture app.
A replacement explicit predicate in development build 818.1 passed synthetic Foundation checks but suppressed Axon for every fixture on the signed devices.
That predicate and its inadequate synthetic activation tests were removed.
Recovery build 818.2 restored the dictionary.
Rebased development build 818.3 was installed on both devices and passed the signed-artifact checks.
The user confirmed Safari sharing works after that recovery; a separate iPad recovery confirmation remains pending.
System share-sheet measurements on the iOS 26.5 simulator reproduced the URL/PNG exclusion with dictionary and simple predicate policies.
A diagnostic receiver confirmed the public types were `public.url` and `public.png` on one provider; removing the PNG representation restored the text/URL destinations.
The fixture therefore does not establish whether a source-supplied thumbnail can reach Axon.
The public preview-image handler is also present when the source supplies no custom preview, and `loadPreviewImage` can fall back to Quick Look.
Axon does not call it automatically or infer source consent from its presence.
The NYT app subsequently exposed a text-provider interoperability failure: its public input had attributed text plus a provider advertising plain text, without a URL representation.
The attributed text imported, but failure to read the provider's representation blocked Send.
The importer now requests concrete public text types, supports declared UTF-16 text, and falls back to the typed public string reader for unavailable/unusable files.
It preserves accompanying prose, deduplicates the URL, and continues to block oversized text and genuinely unsupported attachments.
Eight iOS regression tests cover string-only providers, accompanying prose, UTF-16 input, unusable file fallback, both file/string size enforcement, and legacy web/file URL objects registered as plain text.
Development build 818.4 passed the signed-artifact checks, but the user confirmed the NYT import still failed.
Its device diagnostics recorded an unusable file representation followed by a failure of the newer public string reader.
The importer now also tries the older public item reader, accepting bounded strings, attributed text, and validated web URL objects while rejecting file URLs and opaque data.
It does not inspect or decode private archives.
Signed build 818.5 confirmed that the NYT provider returns a normal string through that older public API.
The user then encountered the 20-draft inbox limit accumulated during testing, discarded unwanted drafts, and confirmed a fresh NYT share delivered successfully.
The full-inbox failure now has a distinct category and message, and its regression test verifies that existing drafts remain intact and an explicit discard frees a slot.
A simulator test with a verified listening socket also confirmed that importing a legacy web URL under a plain-text type does not contact the source URL.
For the missing NYT image, the user reported the earlier combined “Preview generation is disabled or unavailable” status.
The native sender now preserves each of the six nonfatal generation statuses rather than collapsing them, with tests verifying plain-link fallback and no thumbnail request for every status.
Development build 818.7 includes those distinctions and the full-inbox message, passed signed-artifact checks, and is installed on both devices.
The cause of the earlier closure has not been established by a termination report.

Successful source-supplied recipient image-card delivery, source activation checks, complete cold/warm/suspended coverage, and VoiceOver/Dynamic Type remain acceptance gates.
The simulator/provider results above do not establish those physical-device results.

## Device-first revision verified on 2026-11-02

The Share Extension and Pending shares recovery now reuse supplied previews and use on-device acquisition for explicit Include preview requests.
Homeserver lookup has a separate action and confirmation, with no automatic fallback from a failed device lookup.
The device adapter retains validated public image representations, converts generic images to the existing thumbnail format when needed, and preserves ownership/session fences and plain-link fallback.
Native verification passed 58 macOS tests, 84 iOS simulator tests, and the two real share-sheet UI tests.
The new cases cover supplied-card reuse, lookup without account credentials, retained images, generic-image conversion, unavailable/oversized images, cancellation, session loss, stale revisions, and obvious local-address rejection.
The web suite passed 2,912 tests with two skipped.
The five recovery browser cases passed in Chromium and the iPhone WebKit profile, including an assertion that device success and failure issue no homeserver preview request and that submitting the URL requires the separate confirmed action.
The other 253 Chromium checks also passed; the full run's new fixture initially used an incorrect textbox locator, which was corrected and passed in the focused rerun.
API drift and formatting/lint checks passed.
A separate manual network probe on the iOS 26.5 simulator used the production device adapter on the NYT article without a Matrix credential and retained a valid 113,107-byte thumbnail with a matching article title.
That probe is separate from the deterministic CI suite and does not establish website-session reuse or signed-iPhone behavior.
Clean development build 818.8 passed the exported-artifact checks and was installed on the connected iPad.
The user installed 818.8 locally on their remote iPhone and confirmed that the new preview order works correctly.

### Last-destination restore — November 2

The native store now remembers destination changes in a protected, session-bound device preference shared with the extension.
New composers restore the selection after loading current destinations; a missing room clears only the room choice, and a missing account clears both choices.
The preference is separate from each draft's destination and never dispatches a message.
Tests cover persistence across store instances and draft cancellation, partial account selection, unavailable accounts/rooms, credential refresh, server changes, account removal, sign-out, stale writers, and corrupt or unwritable preferences.
Verification passed 63 macOS tests, 89 iOS simulator tests, both real system share-sheet UI tests, and formatting checks.
Clean development build 818.9 passed the exported signing, provisioning, activation, and entitlement checks and is available for remote iPhone installation.
The user installed 818.9 and confirmed that account and room restoration works correctly.

### Pending-share inbox controls — November 2

The toolbar entry now appears only for a nonempty inbox, with a count badge, or an unreadable inbox, with a warning badge and a path to retry.
Recovery offers confirmed **Clear all** for the captured set of drafts and saved delivery records.
The native dispatch checks protect in-flight sends; idle composers are fenced when explicitly discarded, new arrivals are excluded, failures do not stop other deletions, and session changes stop the remaining batch.
Verification passed 2,921 web unit tests with two skipped and all seven recovery browser checks in Chromium and the iPhone WebKit profile.
The broader Chromium run had 257 passing checks and eight skipped; two trace-output collisions and an ambiguous new status locator were resolved and the affected cases passed in the isolated twelve-case rerun.
Lint, formatting, and the production web build passed.
Clean development build 818.10 passed the exported signing, provisioning, activation, and entitlement checks and is available for remote device installation.

### Restricted device preview acquisition — November 2

After the network-boundary review, the user chose a restricted device fetcher instead of Apple-managed URL acquisition.
Supplied Apple metadata remains supported without a lookup, and the homeserver option still requires separate confirmation.
The shared native reader now validates and pins every public HTTPS destination, verifies TLS for the original host, and parses inert metadata with pinned SwiftSoup 2.14.3.
An opt-in live probe fetched a title and valid 64,684-byte thumbnail for the user's original NYT example on macOS and the iOS simulator, without a Matrix credential or send.
The deterministic suite covers address boundaries, redirects, DNS rebinding, blocked images, framing limits, cancellation, inert parsing, and certificate hostname/chain/expiry validation.
The shared package also compiled for arm64 iOS 15 with application-extension API restrictions enabled.
Verification passed 79 macOS tests and 109 iOS simulator tests (one opt-in network test skipped in each deterministic run), plus both real share-sheet UI tests.
The opt-in NYT fetch was run separately on macOS and iOS; deterministic certificate tests cover wrong-host, expired, and untrusted chains.
The web suite passed 2,938 tests with two skipped, and all eight recovery cases passed in Chromium and the iPhone WebKit profile.
API drift, lint/formatting, and the production build passed.
Clean development build 818.12 passed signing, provisioning, activation, and exported-artifact checks and is available for remote phone installation.
