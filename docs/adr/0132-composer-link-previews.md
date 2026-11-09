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
- Lookups start only from the user's own input.
  A draft restored when a room opens gets cards only after the user edits it, so opening a room never causes a fetch.
- No cards in edit mode, for a message with attachments, or for a message containing a spoiler, which the renderer would hide anyway.
  The thread composer behaves the same as the room composer.

### 6. What is sent

Cards that are still in the text at send time decide the field:

- One or more kept, ready cards: `url_previews` carries them in text order.
  Device images are uploaded through the existing media upload first, in parallel, and referenced by `image_upload_id`; a homeserver result already has one.
  If an image upload fails, that card is sent without an image.
  If the list exceeds the 32 KiB budget, images are dropped first, then trailing cards.
- Every card discarded: `url_previews: []`, so recipients' clients know the sender wanted none and need not ask a homeserver.
- Setting off, or nothing ready (still loading, or failed): the field is omitted, exactly as today.

A lookup in progress never delays sending.

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
3. **Web:** the setting, the composer cards, the send wiring, and updates to `docs/bundled-link-previews.md` and `docs/client-parity.md`.

Both implementation PRs are in the `clients/web` silo, and each carries its own tests.
