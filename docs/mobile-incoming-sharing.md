# Incoming sharing on iOS and Android

Status: native sharing proposed; server send/generation prerequisites implemented separately, with no native-device validation yet.
Tracking issue: [564](https://github.com/matrix-axon/matrix-axon/issues/564).
Proposed architecture decision: [ADR 0125](adr/0125-ios-first-incoming-sharing.md).
Implement iOS first because it serves most current mobile users; Android follows the completed iOS media stage.
This document describes the planned native experience; implemented server prerequisites are linked below.

## Experience and scope

From another app, choose Share → Axon → account and room → review → Send.
On iOS, selection and sending happen inside a native Share Extension without requiring the main app to run.
On Android, the system opens Axon and the shared Preact picker/composer handles review and sending.
Selecting Axon never sends immediately or assumes that the room currently open in the app is the intended destination.

Support plain text (usually a hyperlink), rich links with title/description and an optional thumbnail, a web URL with accompanying text, one photo or file, multiple photos or files, and mixed attachment batches with accompanying text.
Videos and other regular files use the existing file-send capability; accepting a video does not imply inline playback or transcoding.
Retain provider order, filenames, and useful accompanying text.
The initial destination is a joined room under an explicitly selected Matrix account; selecting threads, creating rooms, suggested conversation targets, rich HTML import, and an arbitrary-origin crawler in Axon are outside this plan.
Show account identity even when only one account exists, to make the sender clear.

The iOS text/rich-link stage advertises only text/web URLs; a thumbnail belonging to a link is preview metadata, not a separately shared photo.
The media stage adds activation rules for supported attachment types only when their complete flow works.
Reject an unsupported or over-limit selection explicitly; never silently send a subset.
The user may remove problematic items and confirm the remaining content.
Cancellation before dispatch sends nothing; cancellation during dispatch cannot recall a request already accepted by the server.

## Current implementation and prerequisites

Both packaged apps wrap `clients/web` in Tauri.
The platform boundary is `clients/web/src/platform/index.ts`; it currently has no incoming-share port.
Its browser `navigator.share()` path is outbound sharing and does not register the packaged app as a destination.

| Existing code                                                          | Reuse or required change                                                                                                                                        |
| ---------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `clients/web/src/media/attachment-staging.ts`                          | Existing scoped staging and count/byte caps; currently memory-only, unsuitable as the durable inbox                                                             |
| `clients/web/src/components/use-message-composer.ts`                   | Existing room/thread submit behavior; reuse on Android and main-app recovery                                                                                    |
| `clients/web/src/stores/timeline.ts`                                   | Sequential multi-file sends, caption on the first file, independent item outcomes; extend with an optional durable progress sink rather than copy this pipeline |
| `clients/web/src/media/media-service.ts`                               | Existing upload endpoint, file classification and upload budgets; share fixtures with the native adapter                                                        |
| `clients/web/src-tauri/native-auth/ios/Sources/NativeAuthPlugin.swift` | Current app-private Keychain and reinstall cleanup; needs extension-safe shared session access                                                                  |
| `clients/web/src-tauri/Entitlements.ios.plist`                         | Currently Apple sign-in and push entitlements; no sharing App Group or explicit shared Keychain group                                                           |
| `scripts/package-ios.sh`                                               | Deletes and regenerates `gen/apple`; extension targets and signing must be installed reproducibly after generation                                              |
| `clients/web/src-tauri/android/MainActivity.kt`                        | Custom activity currently handles WebView configuration/insets; preserve that behavior and existing notification/OAuth intent handling                          |
| `clients/web/src-tauri/build_support/android_manifest.rs`              | Existing deterministic manifest transformations; extend for share registration                                                                                  |

Existing `/v1/accounts/{account_id}/rooms/{room_id}/send`, media upload and `send-media` endpoints provide the basic send capability.
Merged PR [740](https://github.com/matrix-axon/matrix-axon/pull/740) added text-only bundled link-card rendering in `clients/web/src/link-previews.ts` and `components/LinkPreviews.tsx`.
It does not generate previews or render their images.
Merged PR [833](https://github.com/matrix-axon/matrix-axon/pull/833) added the server-only typed send contract and persistent/encrypted staged thumbnails.
The opt-in homeserver generation helper and authenticated staged-thumbnail retrieval are documented in [bundled link previews](bundled-link-previews.md#opt-in-homeserver-generation).
Stage 4 adds shared web image-card rendering through authenticated media, including encrypted previews, with fixed layout, bounded downloads, and text fallback.
Native sharing remains a later stage; web rendering does not register an OS share target or expose generation in the composer.
`SendMessageRequest` and `SendMediaRequest` currently expose no caller idempotency key or operation-status lookup.
`crates/axon-api/src/routes/messages.rs` claims a staged upload before dispatch and returns the resulting event ID; this does not make a lost HTTP response safely replayable.
Do not infer delivery from a later message with matching text or filename.
The v1 design therefore preserves uncertain results for user resolution instead of promising automatic exactly-once delivery.
Automatic recovery of uncertain dispatches would require a separately designed, additive server prerequisite PR before dependent client behavior.
The server prerequisites and web image-card renderer are available for the later native sharing stages; those adapters must consume the settled contracts.

## Rich links across both platforms

A rich-link share has three independent parts: acquiring metadata, delivering it inside the Matrix message, and rendering the bundled card.
A card visible in the sending app or system share sheet does not prove the receiving app gets its metadata or thumbnail.
Keep the original HTTP(S) URL in the message body or first attachment caption; the card is additional context and removing it never removes the link.

On iOS, Apple's Link Presentation framework represents rich links with `LPLinkMetadata` and displays them with `LPLinkView`.
Import compatible source-supplied title/URL/image data when actually available through supported extension input representations.
Do not assume `UIActivityItemSource` metadata used to decorate the share sheet is always delivered to a third-party extension, and do not use private archive formats or private description fields.
Measure Safari, another browser and a sender that supplies metadata on a signed device; record actual input types and available public metadata without logging content.
The extension shows a native rich-card preview using the normalized data before Send.

Android has analogous Sharesheet text previews: senders can supply a title and thumbnail, with documented support starting at Android 10.
Inspect eligible `EXTRA_TITLE`/`EXTRA_SUBJECT` and granted preview URI/`ClipData` data, but treat them as untrusted optional hints rather than a universal link-metadata object.
Distinguish a link's thumbnail from a standalone `EXTRA_STREAM` photo; do not accidentally send the card image as an additional media message.
When a sender's payload is ambiguous, retain the file as an attachment for review instead of silently reclassifying it.
The Android picker/composer and main-app recovery UI use the shared card model and renderer.

For URL-only shares, offer an explicit **Include preview** action or a previously enabled generation preference.
Both operating systems use the same authenticated Axon helper, which delegates acquisition to the selected account's configured homeserver `preview_url` capability.
Source-provided metadata is reused first, avoiding an unnecessary lookup.
Explain that generating missing metadata discloses the URL to the homeserver; receiving a share or rendering an event never triggers a lookup by itself.
Apple's `LPMetadataProvider` can fetch metadata, but v1 does not add a separate automatic iOS acquisition policy alongside the common service.
If homeserver previews are disabled, generation times out, or no useful metadata is returned, preserve the URL and offer plain-link sending; do not promise every webpage can produce a card.

The settled [server send contract](bundled-link-previews.md) carries typed, bounded optional `url_previews` on text and media-caption sends.
Each entry contains `matched_url`, optional allow-listed `title`, `description` and `site_name` fields, and an optional account-owned `image_upload_id`.
An absent field preserves old callers' behavior; an explicit empty list means no bundled preview.
The server verifies that each matched URL occurs in the outgoing body/caption and converts accepted preview-image bytes into persistent Matrix media.
Emit MSC4095-style `m.url_previews` metadata using its documented field names, checking the proposal/compatibility status at implementation time.
Use `og:image` for persistent unencrypted Matrix media or `matrix:image:encrypted` for encrypted preview files, with byte count/type/dimensions; encryption belongs in `axon-sync`, not the client or reserved `axon-crypto` stub.
Preview-image uploads are not ordinary chat attachments: the existing `send-media` route cannot be used just to obtain an image URI, because it also sends an event.
Generated homeserver thumbnails are staged under the selected account and reuploaded on send for persistent ownership and encrypted-room protection.
Keep the generated thumbnail in account-scoped staging until it is used, discarded or expires; do not leave an unbounded server-side preview cache.
Before marking a preview durably recoverable, copy its thumbnail bytes into the share's protected local staging root, for both source-supplied and generated thumbnails.
The implemented generation helper provides bounded authenticated retrieval of staged thumbnail bytes for this purpose; see the [recovery contract](bundled-link-previews.md#durable-thumbnail-recovery).
Retain the local copy for the unfinished share's lifetime, independently of the server upload's TTL.
On resume, re-upload that copy when its server handle has expired; do not fetch the linked page again just to recover the image.
If local bytes are unavailable or corrupt, show that the thumbnail needs replacement and let the user explicitly regenerate it, remove the image, or discard the share; never silently drop it or automatically regenerate it.
Apply the same quota, ownership, protection, expiry and sign-out cleanup to these local bytes as to other staged files.

Extend the shared `LinkPreviews` renderer to resolve only eligible Matrix preview images through the existing authenticated media/decryption boundary.
Never put a remote HTTPS `og:image` directly into an image element or fetch the webpage in the timeline renderer.
Preserve matched-body validation, escaped text, visible destination hostname, authoritative-field precedence, spoiler suppression, compact-header suppression and edit/redaction behavior from PR 740.
An unavailable/invalid image falls back to a text card, and unavailable metadata falls back to the original link.
Do not serialize an Apple-specific archive, platform URI, or local image path into a Matrix event.

Store normalized preview data and thumbnail handles in the durable incoming-share record with provenance (`source` or `generated`) and the exact matched URL.
If the user removes/changes that URL, remove or explicitly regenerate the stale preview; editing another portion of the text must not discard a valid card.
The first file's caption carries link previews for a mixed batch; subsequent files do not repeat them.
Permit preview removal before sending, and record its delivery with the parent message rather than as an independent send operation.
The settled server limits are eight cards, 4096 UTF-16 code units per URL, 300 for title, 1000 for description and 120 for site name, aligned with the current renderer's string-length units.
Also bound preview metadata to 32 KiB per share; count UTF-8 bytes of the canonical JSON preview array, including keys, escaping, punctuation and resolved image descriptors, but excluding thumbnail file bytes.
Count, field and aggregate limits all apply; the first exceeded limit rejects the proposed selection with an explanation, rather than silently truncating fields or dropping cards.
Per-field maxima are ceilings, not a promise that eight maximally sized cards fit: the aggregate byte cap wins, and the final event-size gate below can impose a tighter budget still.
Keep the draft intact and disable Send until the user edits/removes the excess content; show which limit was exceeded and the measured size/count.
The server DTO, native normalizer and web adapter must enforce the same units and precedence through shared boundary fixtures, including multibyte Unicode and JSON-escaping cases.
Bound each thumbnail to 2 MiB/4 megapixels; include thumbnail bytes in pending/batch quotas without consuming a user attachment slot.
These are server ceilings; validate bounded image decode and ownership cleanup on supported devices, and impose lower native budgets where needed.

## Shared contract and ownership

Define a versioned `IncomingShare` manifest with a share UUID, creation time, normalized server identity and opaque session generation, optional chosen account/room, editable text, and an ordered list of attachment records.
Each attachment has a stable item UUID, safe display filename, declared/sniffed type, measured byte count, and an opaque reference to an app-owned staged file.
The manifest also records import failures and per-item delivery state, including confirmed event IDs and server upload IDs when available.
Use a dedicated text operation for a text-only share; with attachments, text is the first item's caption, matching current composer semantics.
Never put credentials, shared text, or file paths in URLs, intent callbacks, analytics, or logs.
Credentials remain in protected credential storage and are never serialized into the manifest.

Ship a schema and fixtures for normalization, limits, item ordering, caption placement and recovery transitions.
Stage 5's wire DTOs and native fixtures must be generated from, or automatically checked against, the settled stage 2/3 OpenAPI contract; independently handwritten matching shapes are insufficient.
The versioned local manifest may add private staging/ownership fields, but its wire projection must validate against those OpenAPI DTOs, including optional/empty preview fields, byte budgets and definite error categories.
Run the same valid/invalid boundary fixtures through the server, Swift native adapter and TypeScript adapter, and fail their relevant checks when a wire-shape or limit changes without a fixture update.
The iOS app plugin and extension share an extension-safe native module for session access, staging and HTTP operations.
The extension UI is native and does not embed the complete Tauri runtime or a second copy of the Preact composer.
The necessary native HTTP adapter remains small, follows OpenAPI, and has contract fixtures rather than reimplementing timeline, gallery, or HTML logic.
Android uses the existing TypeScript media/composer implementation through a new incoming-share platform port.
Represent native files as opaque handles and acquire bytes lazily; do not push a complete batch through base64 bridge messages.

The platform port needs capability discovery, pending-share enumeration, subscriptions, bounded file access, progress persistence, and explicit acknowledgment/discard.
Subscriptions are wake hints; enumeration after startup/resume is authoritative, so early native arrivals are not lost.
Receiving an event or rendering a preview is not acknowledgment.
Only a durable handoff, confirmed completion, or explicit discard can release the native record.
Existing drafts are preserved; offer merge/review or keep the incoming share separate rather than overwrite a room's draft.

## iOS session and destination access

Add an App Group container for content/session metadata and a shared Keychain access group for credentials, with matching app/extension provisioning.
The main app publishes a coherent server/session descriptor using an opaque generation and updates it on login, token refresh, server change and sign-out.
Read the credential and descriptor under the same cross-process coordination, release the lock before network I/O, and revalidate the generation before dispatch and before applying completions.
An in-flight request may finish after sign-out; prevent further dispatches and stale state resurrection rather than claim remote cancellation.
Share metadata must never be a source of an arbitrary authenticated request URL: the endpoint comes from the trusted app-written server descriptor.
Preserve the main app's transport/security policy, including certificate validation and refusal to forward credentials across origins on redirects.

Keep the main app as the only refresh-token owner in v1.
The extension may use an unexpired persisted OAuth access token or a persisted token-paste credential; it does not rotate OAuth refresh tokens or initiate login.
Session-only credentials are unavailable to the extension and must not become persistent just to support sharing.
If credentials are missing, expired, rejected, or inaccessible, offer to keep a pending share and explain that the user must open Axon to sign in/refresh and resume it.
A transport failure preserves the session; only an authentication rejection can invalidate it.
The descriptor/session policy must support existing token-paste and OAuth users without quietly changing Remember Me behavior.

Migrate existing app-private Keychain entries without losing an established login, and delete obsolete copies only after the shared write succeeds.
Do not call the existing `forgetPreviousInstall()` from the extension: its app-private `UserDefaults` marker would be absent and could erase a valid session.
The app owns an installation marker in its private container and establishes the shared installation generation before extension access is enabled.
An extension invoked before the containing app's first launch offers no authenticated send and does not consume inherited Keychain credentials; use a marker that disappears with removal of the containing app and verify that behavior on device.
Sign-out/account removal/server change invalidates matching pending work and credential access; never rebind an old pending item to a new session implicitly.
Credential migration, install-marker semantics, and extension-safe linkage are exit gates for the foundation PR, not assumptions to postpone until release.

Fetch accounts and joined rooms with finite deadlines; display loading/error/retry and preserve the imported content.
Cached names may help initial rendering, but select/confirm against current server state and handle a room becoming unavailable or unwritable.
Do not assume a joined room guarantees send permission; the server remains authoritative.
Do not put bearer tokens into App Group defaults or copy them into a content file for convenience.

## Media import and resource budgets

On iOS, enumerate `NSExtensionItem` attachments and choose one supported `NSItemProvider` representation per logical item.
Text and URL representations of the same item must not duplicate the hyperlink; preserve separate substantive text.
Copy a loaded temporary file before the provider callback ends; when a URL requires scoped access, balance access start/stop and finish copying while access is valid.
Assign item order before asynchronous loads, so completion timing cannot reorder a batch.
Treat folders, remote link URLs presented as files, and unsupported provider objects as explicit import failures.
Do not fetch a web URL merely because it was shared; metadata generation follows the explicit rich-link policy above.

Start with these proposed budgets, subject to measurement on supported iPhones:

| Resource                 | Initial limit                                                                                  |
| ------------------------ | ---------------------------------------------------------------------------------------------- |
| Attachments              | 10 per share, consistent with current staging                                                  |
| File and batch bytes     | 100 MiB per file and 100 MiB total per share; honor lower server limits                        |
| Text import              | 64 KiB UTF-8 staging ceiling; actual sendability also requires the final event-size gate below |
| Pending storage          | 200 MiB total and 20 share records; reserve space while importing                              |
| Retention                | 24 hours for unfinished shares, with visible expiry and explicit discard                       |
| Concurrent import/upload | One file at a time initially; streaming copies and file-backed uploads                         |

Check limits against measured bytes while copying, not just a provider's advertised size.
Enforce count/metadata limits before opening files, cap filenames, sanitize paths, and disallow traversal and symbolic-link escape from the staging root.
Sniff supported image types; use bounded thumbnail decoding and send other regular files through the file path when inline rendering is unsupported.
Measure HEIC and large video behavior; do not convert an entire photo batch in memory.
Place content in protected app-owned storage excluded from backup, with no credentials in files and no plaintext copies outside that staging root.
Persist import intent, reserve quota, write each file to a temporary path, and atomically publish the manifest only when its referenced files are complete.
Use coordinated cross-process mutations (for example a file lock plus atomic rename); never hold the lock while waiting on a provider or network.
Boot/resume reconciliation removes incomplete imports/orphaned temporary files and exposes recoverable work without depending on an in-memory owner.
Quota/expiry cleanup never deletes an item actively owned by another process; expired ownership is reconciled after termination.
Do not silently evict an unexpired user draft to make room for a new share.

### Event-size validation

The [Matrix event limit](https://spec.matrix.org/latest/client-server-api/#size-limits) is 65,536 bytes for the complete canonical JSON federation event, including signatures; it is not a plain-text body allowance.
The 64 KiB text-import ceiling bounds local work only and never guarantees that imported text can be sent.
Before upstream event dispatch, stage 2 must enforce a documented conservative serialized-content budget that reserves room for federation envelope/signatures and accounts for encryption expansion in encrypted rooms.
Include `body`, any `formatted_body`, preview arrays, image descriptors and relation fields in that budget; Unicode and JSON escaping must be measured as serialized bytes, not character counts.
Finalize the reservation and wire error in the server contract, and check the corresponding native/web preflight against its OpenAPI fixtures before enabling Send.
An oversized draft stays editable, with a specific explanation and choices to shorten text or remove formatting/cards; never silently truncate it or send only a subset.
Server preflight rejection must happen before claiming uploads or dispatching the event and return a documented definite size-validation error.
An explicit homeserver event-too-large rejection is also a definite failure; preserve the draft and return it to review rather than mark it outcome unknown.
This exception requires evidence that dispatch was rejected: a generic timeout, disconnect or upstream failure after dispatch remains uncertain.
Stage 2/6 acceptance includes near-limit ASCII, multibyte and heavily escaped text, rich formatting, multiple cards and encrypted-room overhead.

## Delivery and recovery

Persist user confirmation and the exact destination before dispatch.
Use sequential per-file upload/send, applying the caption to the first item only, consistent with the existing web batch behavior.
There is no atomic multi-file Matrix album transaction; partial delivery must be visible.
Every request, provider load and ownership lease has a finite timeout; cancellation releases local resources and preserves honest remote outcomes.
Use file-backed native uploads and a size-aware upload deadline; the existing web budget is a starting point, not a measured extension lifetime guarantee.

| Persisted state        | Restart or next action                                                                                                     |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Importing              | Remove incomplete temporary data, report incomplete import; never dispatch it                                              |
| Ready                  | Offer review/send; no automatic send without confirmation                                                                  |
| Uploading              | Preserve local file; interrupted upload may leave an unclaimed server upload until its existing expiry                     |
| Uploaded               | Preserve upload ID; an expired upload requires a new upload, not loss of local bytes                                       |
| Preview upload expired | Re-upload the protected local thumbnail; if unavailable, require a visible replacement/removal choice before sending       |
| Dispatching            | On interruption, mark outcome unknown; do not automatically resend                                                         |
| Sent                   | Persist event ID before cleanup; do not send this item again                                                               |
| Failed                 | Distinguish definite validation failure from uncertain remote outcome; permit explicit retry when safe                     |
| Outcome unknown        | Explain that delivery could have occurred; offer inspection and an explicit resend choice that acknowledges duplicate risk |
| Discarded or expired   | Release local files and best-effort delete known unclaimed uploads; no dispatch                                            |

Any timeout, disconnect, or gateway failure after dispatch may have an uncertain outcome, including an API error after upstream acceptance.
A crash after server success but before the event ID is saved is also uncertain.
Persist per-item outcomes so recovering one failed file never resends already confirmed files or repeats a confirmed caption.
Claim a pending operation under the shared owner lock and use a generation/lease to prevent the app and extension from sending it concurrently.
The durable progress sink must observe the real composer dispatch boundary; changing only the incoming-share adapter cannot make the current in-memory local echoes recoverable.

For v1, send while the extension is active and retain incomplete work for explicit app recovery.
Do not call the host's completion handler until confirmed completion or a clearly labeled durable save; show partial delivery and save the remainder when interrupted.
Closing the sheet after saving a draft must not say that the message was sent.
The main app lists pending/partial shares and lets the user resume safely without reimporting source files.
Unattended background transfer chaining is outside v1: an upload finishing in the background does not by itself execute the subsequent `send-media` request.
If large-file device measurements make the foreground flow impractical, revise this decision before shipping rather than add a speculative main-app launch workaround.

## Android adapter

After iOS media acceptance, register `ACTION_SEND` and `ACTION_SEND_MULTIPLE` with the supported MIME types and the proper exported/default intent configuration.
Handle both launch intents and `onNewIntent`, preserving OAuth deep links and notification navigation.
Normalize `EXTRA_TEXT`, `EXTRA_SUBJECT`, `EXTRA_STREAM` and `ClipData`; deduplicate repeated representations without discarding deliberately repeated separate items.
Read content URIs through `ContentResolver` under their temporary grants and copy them promptly into bounded staging.
Do not depend on persistable permissions being granted by every sender or request blanket external-storage access.
Use the shared contract/fixtures, platform port, destination selection, acknowledgment and per-item progress semantics.
The source intent is consumed once, while durable pending records remain retrievable across app recreation.

## PR sequence and acceptance

Each row is one PR, merging in order; each behavioral PR includes its own tests, docs and device evidence.
Stages 2 and 3 are server-only prerequisites, allowing only mechanical generated-schema synchronization across silos.
Later implementation stages stay in the web/native-client silo and consume the settled server contract.

| Stage | Sub-issue                                                    | PR boundary and exit criterion                                                                                                                             |
| ----- | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1     | [816](https://github.com/matrix-axon/matrix-axon/issues/816) | This design, indexed documentation, contract/lifecycle decisions and device measurement plan                                                               |
| 2     | [821](https://github.com/matrix-axon/matrix-axon/issues/821) | Server: typed bundled-preview send support and persistent/protected preview-image media; round-trip encrypted and unencrypted cards                        |
| 3     | [822](https://github.com/matrix-axon/matrix-axon/issues/822) | Server: bounded opt-in homeserver preview generation and image staging, with unsupported-capability fallback                                               |
| 4     | [823](https://github.com/matrix-axon/matrix-axon/issues/823) | Client: shared image-card rendering through authenticated media, preserving PR 740's safety and lifecycle rules                                            |
| 5     | [817](https://github.com/matrix-axon/matrix-axon/issues/817) | Manifest wire projections/native fixtures checked against stage 2/3 OpenAPI; iOS session/staging and app recovery UI; signed App Group/Keychain acceptance |
| 6     | [818](https://github.com/matrix-axon/matrix-axon/issues/818) | iOS Share Extension, native picker/composer and text/rich-link delivery including thumbnails and explicit generation; text/web-URL activation rules        |
| 7     | [819](https://github.com/matrix-axon/matrix-axon/issues/819) | iOS single/multiple media, captions containing rich links and per-item delivery/recovery; enable media activation rules with signed-device acceptance      |
| 8     | [820](https://github.com/matrix-axon/matrix-axon/issues/820) | Android intent ingestion, shared web picker/composer and equivalent rich-link/text/media delivery                                                          |

Every implementation PR updates user/build documentation, `docs/client-parity.md` when tracked status changes, and `docs/demo-coverage.md` for changed rendered flows.
Record rich-link acquisition, bundled sending and image rendering separately; text-card support alone does not complete the end-to-end feature.
Run required client checks and focused native tests per PR; documentation-only work needs formatting/link checks rather than unrelated runtime suites.
The Linux design workspace cannot establish extension signing, iOS lifecycle or memory acceptance.
Before native implementation/verification, continue on the available macOS development machine with Xcode and a signed iPhone build; do not claim a web test proves native registration.

| Verification area | Required evidence                                                                                                                                                   |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| iOS sources       | Safari URL and selected text; Photos single/multiple images and video; Files PDF, mixed/multiple files and a cloud-backed provider                                  |
| Content semantics | Provider order, duplicate representations, text plus files, first-item caption, HEIC fallback, unsupported objects and misleading MIME                              |
| Lifecycle         | Main app cold/warm/suspended, extension killed during import/upload/dispatch, app recovery, competing consumers, repeated shares                                    |
| Identity          | Multiple accounts, stale room selection, login upgrade, expired token, session-only login, sign-out, server switch, reinstall and pre-first-launch extension access |
| Failure           | Offline/slow server, lost success response, partial batch success, rejected upload, low storage, quota/expiry cleanup, cancellation and duplicate-risk resend       |
| Packaging         | Clean regeneration embeds the signed `.appex`; validate both bundle IDs, versions, activation rules, entitlements and provisioning in the exported IPA              |
| Accessibility     | VoiceOver, Dynamic Type, keyboard avoidance, thumbnail memory and usable cancel/send controls on a signed iPhone                                                    |
| Android           | Real-device browser/Gallery/Files shares, single/multiple/mixed content, cold/warm intents, revoked URI access and existing OAuth/notification flows                |
| Shared web UI     | Store/interaction tests plus actual geometry/hit testing on desktop Chromium and iPhone/WebKit                                                                      |

Rich-link acceptance includes actual iOS input representations, Android URL-only/title/thumbnail senders, source metadata versus explicit generation, disabled homeserver previews, URL edits, preview removal and expired provider access.
Round-trip image cards through real encrypted and unencrypted rooms, including media-caption previews, missing images, hostile metadata and interruption during thumbnail upload.
Exercise a server thumbnail upload expiring before a retained local share, successful re-upload from local bytes, and the visible missing-local-copy path without automatic webpage fetching.
Verify aggregate-preview and final-event byte limits with Unicode, JSON escaping and encryption overhead, and distinguish explicit size rejection from ambiguous dispatch failure.
Verify that opening a timeline does not contact the linked site and that an unrecognized source thumbnail is never silently sent as a separate photo.

Log structured share/item IDs, state transitions, byte counts, duration and error categories, with account/room/event identifiers where applicable.
Never log message content, filenames, source URIs, tokens, or provider error strings that could contain private data.
An extra developer-mode option is not initially necessary; sanitized state-transition tracing should diagnose ownership and recovery without exposing content.

## Platform references

- [Apple: Share Extensions](https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/Share.html) documents the extension UI and content-validation model.
- [Apple: extension lifecycle and communication](https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/ExtensionOverview.html) explains the separate lifecycle and restricted containing-app interaction; do not rely on opening Axon from a Share Extension.
- [Apple: common extension scenarios](https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/ExtensionScenarios.html) covers shared containers, coordination, activation rules and background transfers.
- [Apple: sharing Keychain access](https://developer.apple.com/documentation/security/sharing-access-to-keychain-items-among-a-collection-of-apps) covers shared credential access groups.
- [Android: receiving shared data](https://developer.android.com/develop/ui/compose/sharing/receive) documents intent registration and extracting shared text/files; Axon's UI remains Preact rather than Compose.
- [Apple: Link Presentation](https://developer.apple.com/documentation/linkpresentation) describes native link metadata and presentation; source input must still be measured in the receiving extension.
- [Apple: embedding and sharing rich links](https://developer.apple.com/videos/play/wwdc2019/262/) distinguishes fetching metadata from decorating the share sheet.
- [Android: rich text content previews](https://developer.android.com/develop/ui/compose/sharing/send#rich-content-previews) documents title/thumbnail hints; these are optional and must not be mistaken for a guaranteed card payload.
- [MSC4095: bundled URL previews](https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/4095-bundled-url-previews.md) defines the proposed event fields and encrypted/persistent preview-image behavior.
