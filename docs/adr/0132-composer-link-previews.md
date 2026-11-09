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
   In the browser, where there is no device source, a card starts in that state.

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
It builds on html5ever, never runs scripts, and uses the same html5ever 0.39 already in the shell's lockfile.
The image is decoded with the `image` crate, also already in the lockfile, with decoder limits set before allocation.
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

The default is `device` in the shell, and `off` in the browser, where "on this device" is shown as unavailable.
The setting is per device, like Element's encrypted-room settings, because what a device can do and which network it is on differ between devices.
The settings page explains that a device lookup shows the site your address when you type a link, before you send it, and that the homeserver option discloses the link to your homeserver even in encrypted rooms.

### 5. Composer behavior

- A link's lookup starts about 400 ms after the link is complete (followed by whitespace, or pasted), not while the cursor is still inside it.
- Removing a link from the text cancels its lookup and removes its card.
- Each card shows the site's host name prominently, with the title, description and image, and an X that discards it.
  A discarded link is remembered for the rest of that draft and not looked up again.
- Results are cached in memory for about 20 links, so retyping or moving a link does not fetch it again.
  The cache holds normalized metadata and thumbnail bytes, never an `image_upload_id` (see section 7).
- Lookups start only from the user's own input.
  A draft restored when a room opens gets cards only after the user edits it, so opening a room never causes a fetch.
- No cards in edit mode, for a message with attachments, or for a message containing a spoiler, which the renderer would hide anyway.
  The thread composer behaves the same as the room composer.

### 6. What is sent

MSC4095 gives the field three meanings, and the composer chooses between them deliberately.
A non-empty list is the whole set of previews: recipients' clients preview those links and no others.
An empty list means the sender wants no previews.
An omitted field leaves recipients' clients free to scan the body and build their own.

At send time, only cards whose link appears as a complete bare link in the body being sent count; this is the body after composer formatting, the same one the server matches against.
The first rule that matches decides the field:

1. **Setting `off`:** omit the field, exactly as today.
2. **At least one card kept and ready** after the size check below: send those cards, in text order.
   Links that were discarded, still loading, failed or past the third therefore get no preview from any recipient.
3. **At least one card discarded** by the user: send `url_previews: []`.
   One discard is enough, even when other cards are still loading or failed, because omitting the field would let a recipient's client preview the link the user removed, and perhaps disclose it to that recipient's homeserver.
4. **Otherwise** (every card still loading or failed): omit the field.

A lookup in progress never delays sending.

**The size check covers the whole message, not only the previews.**
The server enforces both a 32 KiB preview budget and a 40 KiB limit on the serialized content: body, formatted body, reply and thread relations, and both preview spellings.
So before any image is uploaded, the composer builds the exact request it is about to send and runs the existing `previewPreflight` on it (`incoming-share/contract.ts`, which mirrors the server and already reserves room for each encrypted image descriptor).
Image slots are checked with a placeholder upload id, since the real ids do not exist yet.
If the check fails, the composer degrades in a fixed order and re-checks after each step:

1. Drop the last card's image, then the image of the card before it, and so on.
2. Drop the last card, then the one before it, and so on.

Once a card is dropped for size, the rules above are re-applied to what remains, so a draft with a discarded link can still end up as `[]`.
If even `[]` fails the check, which can only happen when the body alone is near the limit, the field is omitted, and the server's ordinary limits decide the send exactly as they do today.
A preview never turns a sendable draft into a rejected one.

A server rejection of a request that carried previews, despite the check, is a definite failure, not an unknown outcome, and means the client's mirror of the limits has drifted.
That is HTTP 413 with `preview_too_large` or `event_too_large`, or HTTP 400 `bad_request`, which the server returns for invalid preview metadata without a more specific code.
The message stays a failed echo with its usual Retry and Discard actions, and the echo records that its previews were rejected.
Retry then sends the same text with the field omitted, because resending the same previews would only be rejected again.
A 400 caused by something other than the previews loses them on Retry too, which costs only the cards.
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
- The shell gains a dependency (`webpage`, plus `markup5ever_rcdom`); its html5ever and `image` versions are already present.
- Two implementations of the fetch boundary exist, held together by a shared fixture.
- Edits keep their current behavior; editing a message with previews is left for a later decision.
- The TUI remains a gap in `docs/client-parity.md`.
- Telegram-style controls (media size, card above the text) are not included.

## Delivery

1. This ADR.
2. **Shell:** the `axon-link-preview` plugin, its capability, the shared boundary fixture (also read by the Swift tests), and an optional `Platform.linkPreview(url, signal)` binding that the browser leaves undefined.
3. **Web:** the setting, the composer cards, and the send wiring.
   The send wiring includes the timeline store's per-attempt thumbnail uploads, carried as bytes in the send options, and the failed echo's record that its previews were rejected.
   This PR also updates `docs/bundled-link-previews.md` and `docs/client-parity.md`.

Both implementation PRs are in the `clients/web` silo, and each carries its own tests.
