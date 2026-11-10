# ADR 0132 — Composer link previews

**Status:** Proposed. Extends [ADR 0125](0125-ios-first-incoming-sharing.md); supersedes nothing.

## Context

[ADR 0125](0125-ios-first-incoming-sharing.md) built every part of a bundled link preview except the ordinary composer:

- The server accepts `url_previews` on `/send` and `/send-media`.
  It writes both `m.url_previews` ([MSC4095](https://github.com/matrix-org/matrix-spec-proposals/pull/4095)) and `com.beeper.linkpreviews`, and attaches a staged thumbnail, encrypted in an encrypted room ([send contract](../bundled-link-previews.md)).
- `POST /v1/accounts/{account_id}/link-previews/generate` asks the account's homeserver for a preview, through Axon.
- The web client renders received previews in rooms and threads, and never fetches a linked page to do it.
- The iOS Share Extension builds a preview on the device with a restricted HTTPS reader, and offers the homeserver as a separate, confirmed option.

The composer still sends a bare link.
Recipients then see either no card, or a card their own client builds after the fact.
Element Web, for example, asks the recipient's homeserver in unencrypted rooms, and the developer option in Element X iOS fetches the page from the recipient's device.
Either way the recipient discloses the link, or their address, without having chosen to.

A preview built by the sender and sent with the message avoids that.
It is what Signal, WhatsApp and iMessage do, and it is the model MSC4095 standardizes.

The composer is one Preact component on every platform: the browser, and the Tauri shell on macOS, Windows, Linux, iOS and Android.
What differs is whether the client can fetch a web page at all.

- **A browser cannot.**
  CORS hides another origin's response from the page, and Axon's own server does not crawl websites (ADR 0125).
- **The shell can**, but only through code it owns.
  The webview's `connect-src` is limited to the shell, and the shared transport of [ADR 0127](0127-shared-shell-http-client.md) is to be narrowed to Axon-server paths ([issue 893](https://github.com/matrix-axon/matrix-axon/issues/893)).

### No usable operating-system API

Only Apple platforms have one.
`LPMetadataProvider` (iOS 13, macOS 10.15) was the first iOS share implementation, and ADR 0125 replaced it on 2026-11-02 after a network-boundary review.
It cannot be confined to public destinations, it fetches subresources on its own, and it returns no description.
Android, Windows and Linux have no link-metadata API in the platform; every client there fetches and parses the page itself.

### Prior art

| Client                 | Who builds the preview                                    | Composer control                                                   |
| ---------------------- | --------------------------------------------------------- | ------------------------------------------------------------------ |
| Signal                 | Sender's device; HTTPS only, every redirect re-checked    | First link only; X discards it, and that link is not fetched again |
| WhatsApp, iMessage     | Sender's device; sent with the message                    | Discard                                                            |
| Telegram               | Telegram's servers                                        | Discard, choose the link, media size, above or below the text      |
| Slack, Discord         | Their servers                                             | Discard; `<link>` suppresses an embed in Discord                   |
| Element Web            | Homeserver `/preview_url`, bundled behind a labs flag     | Per-room and per-device settings; encrypted rooms off by default   |
| gomuks, Sable, Beeper  | Bundled with the message (MSC4095 or its Beeper spelling) | Sable lists "links that will be previewed" and can remove them all |
| Element X iOS (option) | Recipient's device, with `LPMetadataProvider`             | None; the pattern this ADR avoids                                  |

MSC4095 adds the rules this ADR relies on: an empty `m.url_previews` means the sender wants no previews, a preview's URL must appear in the body, a client should only generate previews for text the user wrote, and a fetch should refuse non-public addresses after resolving DNS.

## Decision

### 1. The composer authors bundled previews, up to three per message

Each distinct eligible link in the draft gets its own card, in the order the links appear, up to three.
Two links are the same link when their WHATWG URL serializations are equal: `new URL(link).href` in the composer, which is what `previewPreflight` compares, and the `url` crate's serialization on the server, which rejects a send whose previews name one destination twice.
So host case, a default port, an empty path versus `/`, and the percent-encoding the parser normalizes all collapse to one link, while a different fragment, or a trailing slash on a non-empty path, does not.
A repeated link shares the card of its first occurrence, whose exact text is the card's `matched_url`, and discarding that card discards every occurrence.
The fourth and later links get no card.
Discarding a card does not give its slot to a later link, which would surprise the user and cost another fetch.
Three is a composer limit, well inside the server and renderer cap of eight and the 32 KiB metadata budget.

A link is eligible when the existing static check accepts it (`nativePreviewLinks`, today in `incoming-share/urls.ts`): HTTPS, a public-looking DNS name with no IP literal, no credentials, no internationalized label, and no reserved suffix such as `.local`, `.onion` or `.test`.
That check moves to a shared module, because the composer and share recovery must agree on it.

### 2. Two sources, in a fixed order, never escalating on their own

1. **This device.**
   Available only in the shell.
2. **The homeserver**, through the existing `/link-previews/generate`.
   Used only when the user taps "Try homeserver" on a card that the device could not fill, and only if the setting allows it.
   In the browser, where there is no device source, a card starts in that state when the setting allows the homeserver.

The homeserver is never asked automatically.
ADR 0125's reason holds here: its plaintext request can enter the homeserver's logs even when the destination room is encrypted, so each disclosure is a separate, explicit choice for that link.

### 3. One restricted fetcher in Rust, for all five shell platforms

A new in-repo Tauri plugin, `axon-link-preview`, fetches and parses a page and its preview image.
It is a separate plugin from `axon-http`, so it neither depends on the transport's any-URL reach nor blocks narrowing it.

It enforces the same boundary as the Swift reader in `incoming-share/Sources/AxonIncomingShare/PreviewNetwork.swift`, rule for rule:

- **Hosts:** the static check above, repeated natively for the first URL and every redirect and image URL.
- **DNS:** query only the origin's A records, and require a public IPv4 address.
  An origin's AAAA records are never used, because a global IPv6 address can be an operator's NAT64 mapping of a private IPv4 service.
  On an IPv6-only network, a public IPv4 target is reached through a NAT64 prefix discovered with RFC 7050 (`ipv4only.arpa`), as `PreviewNAT64.swift` does.
- **Connection:** connect only to the validated address, so a second resolution cannot rebind it.
  TLS 1.2 or later, verified against the original host name, with no certificate-related network fetches and no proxy.
- **Request:** a plain `GET` with no cookies, credentials, or auth headers, and `Accept-Encoding: identity`.
- **Redirects:** followed by hand, at most six hops, each one validated as a new request.
- **Responses:** only status 200 is used.
  Headers are capped at 32 KiB and an image at 2 MiB.
  A page is read only until the end of its `<head>` or 256 KiB, whichever comes first, and is not parsed if those bytes hold more than 512 `<` characters.
- **Time:** one 25-second deadline covers the page, its redirects and its image, and the caller can cancel at any point.
- **Concurrency:** at most two lookups at once across the app.

Metadata uses the same fields in the same order as the Swift reader:

- title: `og:title`, `twitter:title`, `<title>`
- description: `og:description`, `twitter:description`, meta `description`
- site name: `og:site_name`
- image: `og:image:secure_url`, `og:image`, `twitter:image`, `twitter:image:src`

Each text field is normalized as `TextShare.previewField` does: whitespace is trimmed, and a value that is empty or longer than the send contract's limit (300, 1,000 and 120 UTF-16 code units for title, description and site name) is dropped, not truncated.
The composer applies the same rule again to every card, from either source, before it can be sent, so an out-of-range field can never reach the server.

The page is parsed with the [`webpage`](https://crates.io/crates/webpage) crate (MIT), built without its `curl` feature so it only parses what the plugin fetched.
It builds on html5ever and never runs scripts.
`webpage` 2.1.0 requires `html5ever` ^0.39 and `markup5ever_rcdom` ^0.39; the shell's lockfile already has `html5ever` and `markup5ever` 0.39.0, so only `markup5ever_rcdom` is new and the parser is not duplicated.
The image is decoded with the `image` crate, whose 0.25 release is already in the lockfile, with decoder limits set before allocation.
The lockfile builds it only with its PNG and TIFF codecs, so the plugin enables JPEG, GIF and WebP decoding and JPEG encoding, which adds those codec crates.
It is scaled to at most 1200 pixels on its longest side and re-encoded as JPEG, or PNG when it has transparency, so it always fits the upload contract (2 MiB, 4,000,000 pixels).
Re-encoding also drops EXIF and other embedded metadata.
SVG and animated formats are not accepted as previews; a GIF contributes its first frame.

**Why not reuse the Swift reader on iOS?**
One composer path is simpler to reason about and to test, and the Rust plugin already has to exist for the other four platforms.
The Share Extension keeps its Swift reader, because it is a separate process with no Rust runtime.
That leaves two implementations of one boundary, which the "no duplicate code" rule exists to prevent.
So the host, address and NAT64 cases become one shared fixture that both test suites read, and a change to either implementation that disagrees with it fails a test.

### 4. Per-device setting with three values

`linkPreviews` in the web settings store (localStorage), with these values:

- `off`: no cards, no fetches.
- `device`: build previews on this device only.
- `device+homeserver`: as `device`, plus the per-card "Try homeserver" button.

A value names what the user permits; a source the platform lacks is unavailable rather than an error.
The shell offers all three values.
The browser offers two: Off, and "Homeserver, when I ask", stored as `device+homeserver`, with "on this device" shown as unavailable.
A browser card therefore starts in the "Try homeserver" state, and a stored `device` behaves as `off` there.
Stored values do not move between the browser and the shell, because each has its own origin and so its own localStorage, but this rule makes any stored value safe on either.
The default is `device` in the shell and `off` in the browser.
The setting is per device, like Element's encrypted-room settings, because what a device can do and which network it is on differ between devices.
The settings page explains that a device lookup shows the site your address when you type a link, before you send it, and that the homeserver option discloses the link to your homeserver even in encrypted rooms.

### 5. Composer behavior

- A typed link's lookup starts about 400 ms after the link is complete, that is, followed by whitespace, not while the cursor is still inside it.
  A pasted link's lookup starts at once, as in Signal, so the site sees the user's address before the card can be discarded.
  Deferring the pasted case is tracked in [issue 1031](https://github.com/matrix-axon/matrix-axon/issues/1031).
- Removing a link from the text cancels its lookup and removes its card.
- Each card shows the site's host name prominently, with the title, description and image, and an X that discards it.
  A discarded link is remembered, by link identity, for the rest of that draft: it is not looked up again and gets no new card, even if the user deletes it and types it again.
  That record only governs cards; what is sent depends on the body alone (section 6).
- Results are cached in memory for about 20 links, so retyping or moving a link does not fetch it again.
  The cache holds normalized metadata and thumbnail bytes, never an `image_upload_id` (see section 7).
- Lookups start only for links the user adds.
  A link is added when an edit makes it appear in the text, by typing, pasting, or changing an existing link.
  Links already present in a restored draft are not added: they get no card and no lookup, even after the user edits other text, until the user retypes or re-pastes them.
  Opening a room or editing unrelated words therefore never causes a fetch.
- No cards in edit mode, for a message with attachments, or for a message containing a spoiler, which the renderer would hide anyway.
  The thread composer behaves the same as the room composer.

### 6. What is sent

MSC4095 gives the field three meanings, and the composer chooses between them deliberately.
A non-empty list is the whole set of previews: recipients' clients preview those links and no others.
An empty list means the sender wants no previews.
An omitted field leaves recipients' clients free to scan the body and build their own.

The body being sent is authoritative: this is the body after composer formatting, the same one the server matches against, and only cards whose link appears in it as a complete bare link count.
The first rule that matches decides the field:

1. **Setting `off`, or a send the composer does not author previews for** (an edit, a message with attachments, or one containing a spoiler; section 5): omit the field, exactly as today.
2. **At least one card kept and ready** after the size check below: send those cards, in text order.
3. **The body contains any eligible link** (section 1): send `url_previews: []`.
   This covers a link whose card was discarded, is still loading or failed, a link past the third, a discarded link the user deleted and typed again, and a link from a restored draft.
4. **Otherwise** (no eligible link in the body): omit the field.

So once previews are on, an eligible link is previewed only from a card the user kept.
Omitting the field instead would let a recipient's client build a preview the sender never saw, and perhaps disclose the link to that recipient's homeserver.
The cost is that a link sent before its lookup finishes, or whose lookup failed, has no preview for anyone, as in Signal.
Links the static check rejects, such as plain `http` links, get no card and do not trigger rule 3, so a message containing only those still omits the field.

A lookup in progress never delays sending.

**The size check covers the whole message, not only the previews.**
The server enforces both a 32 KiB preview budget and a 40 KiB limit on the serialized content: body, formatted body, reply and thread relations, and both preview spellings.
So before any image is uploaded, the composer builds the exact request it is about to send and runs the existing `previewPreflight` on it (`incoming-share/contract.ts`, which mirrors the server and already reserves room for each encrypted image descriptor).
Image slots are checked with a placeholder upload id, since the real ids do not exist yet.
If the check fails, the composer degrades in a fixed order and re-checks after each step:

1. Drop the last card's image, then the image of the card before it, and so on.
2. Drop the last card, then the one before it, and so on.

Once every card is dropped for size, rule 3 applies and the field is `[]`.
Even `[]` can fail the check, but only when the body alone is near the limit, because the ordinary send path without the field allows a larger body.
If the user discarded none of the body's links, the field is then omitted, and the server's ordinary limits decide the send exactly as they do today.
If the user discarded one of them, omitting the field would expose that link, so the composer does not do it silently: it says the message is too long to carry its "no previews" marker, and the user either shortens it or sends it without the marker.
This is the only use of the discard record at send time, and it counts only discarded links present in the body being sent.
A preview therefore never turns a sendable draft into a rejected one, and a discarded link is never exposed without the user's choice.

A server rejection of a request that carried previews, despite the check, is a definite failure, not an unknown outcome.
These responses count as a preview rejection:

| Response                | Cause on a preview send                                                                                                   |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| 413 `preview_too_large` | The 32 KiB preview budget                                                                                                 |
| 413 `event_too_large`   | The 40 KiB content budget, which means the client's mirror has drifted, or an explicit size rejection from the homeserver |
| 413 `payload_too_large` | A thumbnail's bytes, when the server claims the upload                                                                    |
| 400 `bad_request`       | Invalid preview metadata or image; the server has no more specific code                                                   |
| 404 `not_found`         | A thumbnail upload the server could not claim, because it expired, was consumed, or belongs to another account            |

The message stays a failed echo with its usual Retry and Discard actions, and the echo records that its previews were rejected.
Retry then sends the same text with no cards, so rule 3 sends `[]`.
Resending the same previews would only be rejected again, and omitting the field would hand the links back to recipients' clients, so Retry does neither.
If the rejected request already carried `[]` and failed for size, Retry applies the size check above to `[]`, including its question when a discarded link is in the body.
One of these codes caused by something other than the previews, such as a 404 for the room, loses the cards on Retry too; that Retry fails for the same reason anyway, so only the cards are lost.
Any other failure, such as a 5xx, a timeout or an unknown outcome, keeps the cards, and Retry uploads their thumbnails again as section 7 describes.
Nothing is retried automatically.

### 7. Thumbnails are bytes until the moment of sending

A staged upload handle is owned by the account, expires on the server's schedule rather than the draft's, and is consumed when a send succeeds.
A handle kept in a cache or a draft can therefore be gone by the time it is used, and the whole send then fails with 404 when the server claims it.
So a card holds its thumbnail as bytes, and a handle exists only for the length of one send attempt:

- **Device result:** the plugin returns the re-encoded bytes.
- **Homeserver result:** the composer immediately reads the generated thumbnail with `GET /v1/accounts/{account_id}/media/uploads/{upload_id}/preview`, the same retrieval share recovery uses ([Durable thumbnail recovery](../bundled-link-previews.md#durable-thumbnail-recovery)), keeps the bytes, and forgets the handle, leaving that staged upload to expire.
  If the read fails (404, 413, 400 or 503), the card keeps its text and visibly loses its image.
  The page is never regenerated to recover an image.
- **Each send attempt** uploads the kept cards' bytes through the existing media upload, in parallel, and uses the new ids in that one request.
  The bytes travel in the send options, not the ids, so the timeline's Retry on a failed echo uploads them again, the way a failed media send already re-uploads its retained `File`.
  If one upload fails, that card is sent without an image.
  If the parent send fails, its staged uploads are left to expire.

Because Retry rebuilds the uploads, a handle that was consumed or has expired is never sent.
Whether to retry a send whose outcome is unknown stays the user's choice, as it is for every send today.

## Consequences

- Every platform shares one composer flow, and the shell's five platforms share one fetcher.
  Only the browser lacks a device source, and it can still use the homeserver per link.
- Typing a link in the shell contacts that site from the user's device, as in Signal and WhatsApp.
  The setting can turn it off, and nothing is fetched until the user writes a link.
- Recipients see what the sender's device saw.
  Bundled metadata is the sender's claim, which is why the card leads with the link's real host name.
- The shell gains dependencies, each to be checked against `deny.toml` and recorded in `THIRDPARTY.md` in the plugin PR:
  - `webpage` and `markup5ever_rcdom`; `html5ever` 0.39 is already present.
  - The `image` crate's JPEG, GIF and WebP codecs; the crate itself is already present.
  - A resolver that, on every one of the five platforms, sends only A queries for the origin and a separate AAAA query for `ipv4only.arpa`, never an AAAA query for the origin.
    Rust's standard `ToSocketAddrs` cannot.
    The plugin PR chooses the implementation by that requirement and proves it on each platform with a test, rather than relying on an address-family hint being honored.
    The candidates are a platform lookup restricted to IPv4, which is what the Swift reader uses (`DNSServiceGetAddrInfo` with `kDNSServiceProtocol_IPv4`), and a DNS client such as `hickory-resolver`, which sends the queries itself but must discover the platform's nameservers, which is harder on Android and iOS.
  - TLS pinned to the validated address while verifying the original host name.
    `rustls` 0.23 and `tokio-rustls` are already present, and rustls takes the server name separately from the socket, so this should need no new crate.
- Two implementations of the fetch boundary exist, held together by a shared fixture.
- Edits keep their current behavior; editing a message with previews is left for a later decision.
- The TUI remains a gap in `docs/client-parity.md`.
- Telegram-style controls (media size, card above the text) are not included.

## Delivery

1. This ADR.
2. **Shell:** the `axon-link-preview` plugin, its capability, the shared boundary fixture (also read by the Swift tests), and an optional `Platform.linkPreview(url, signal)` binding that the browser leaves undefined.
3. **Web:** the setting, the composer cards, and the send wiring.
   The send wiring includes the timeline store's per-attempt thumbnail uploads, carried as bytes in the send options, and the failed echo's record that its previews were rejected.
   It adds a fixture of link-identity cases (host case, default ports, empty paths, fragments, trailing slashes, percent-encoding), each with the expected `new URL(...).href`, checked by the web tests.
   This PR also updates `docs/bundled-link-previews.md` and `docs/client-parity.md`.
4. **Server:** a test in `axon-core` that reads the same link-identity fixture and checks it against the `url` crate, so a difference between the two parsers fails a test instead of a send.
   Internationalized host names cannot differ, because the static check in section 1 rejects them.

The shell and web PRs are in the `clients/web` silo, the fixture test is in the server silo, and each carries its own tests.
