# ADR 0138 — Previews of Matrix links, built by the recipient

**Status:** Proposed. Amends [ADR 0132](0132-composer-link-previews.md) §§ 1, 5, 6 and 8; supersedes nothing.
This ADR is the plan only; implementation waits for its review.

## Context

[ADR 0132](0132-composer-link-previews.md) gives each eligible link in a draft a card that the sender builds and sends with the message.
A Matrix link passes its eligibility check: `https://matrix.to/#/!room:server/$event` is an HTTPS link to a public host.
So today the shell fetches the matrix.to page, and the browser offers "Try homeserver" for it.
Neither returns anything useful.
matrix.to is a static page that renders its target with client-side script, so its head names matrix.to itself, never the room or the message.
`matrix:` URIs are not HTTPS, so they get no card at all, and the server would refuse one (`validate_metadata` in `axon-core/src/link_preview.rs` accepts only `http` and `https`).

What a reader wants from a link to a Matrix event is the event: who wrote it, what it says, and its image if it has one.

### Why the sender cannot build that card

A card built by the sender is copied into the destination room.
Every member of that room would then read the linked message, whether or not they can read the room it came from.
If the source room is encrypted and the destination is not, the message would also leave encryption.
A permalink is often shared exactly because the recipient is in the source room too, but the sender's client cannot know which recipients are, and the card cannot be shown to some members and not others.

### What exists already

- **Pills.** The web client already renders a Matrix link to a room or an event as a pill (`resolveMatrixToRoomLink` in `clients/web/src/matrix-to.ts`, used by `FormattedBody.tsx` and `html/sanitize.ts`).
  A pill for a room the account has joined opens it locally, at the event for an event link; any other room's pill offers to join it.
  Both link forms are parsed: `https://matrix.to/#/…` and `matrix:roomid/…/e/…` or `matrix:r/…/e/…`.
- **A single-event read.** `GET /v1/accounts/{account_id}/events/{event_id}` reads one event from Axon's store.
  An event Axon never ingested is a 404, even when the account could read it on the homeserver.
- **Backfill** (ADR 0043) pages every joined room back to its start by default, so an event in a joined room is usually in the store.
  The exceptions are a room still being backfilled, an operator's depth cap or disabled backfill, and a room the account has left, where the homeserver still serves the history it could read while a member.
- **Media is authorized by the event that names it.**
  `GET /v1/media/{account_id}/{server_name}/{media_id}` serves an `mxc://` object only when a stored event refers to it, and takes the encrypted-file descriptor and MIME type from that event (`resolve_media_content` in `routes/media.rs`).

## Decision

### 1. The sender never builds a card for a Matrix link

A Matrix link is any link whose origin is `https://matrix.to`, and any `matrix:` URI, whatever it points at: a user, a room or an event.
The composer gives it no card and no lookup, on any platform, and never offers "Try homeserver" for it.

The rule sits beside ADR 0132's static check, not inside it.
`isPreviewLink` stays the network boundary that the shell's plugin and the Swift reader mirror rule for rule, and the shared fixture (`incoming-share/fixtures/device-preview-urls.json`) describes `isPreviewLink` alone.
The web test that reads the fixture calls `isPreviewLink` rather than `nativePreviewLinks`, so the fixture never has to know about Matrix links, and a separate web-only test covers the exclusion.
A new `isMatrixLink` (in `matrix-to.ts`) is applied together with `isPreviewLink` wherever the web client chooses links for cards: the composer, `nativePreviewLinks`, and therefore share recovery.
Share recovery takes both its "Try homeserver" offers and its check of a staged card from `nativePreviewLinks`, so it neither offers the homeserver for a matrix.to link nor sends a card the iOS Share Extension built for one.
The Share Extension's Swift reader still fetches the page; teaching it the same rule is iOS work, tracked in [#1121](https://github.com/matrix-axon/matrix-axon/issues/1121).

**What is sent** (ADR 0132 § 6).
Matrix links are left out of § 6 entirely: the rules there are applied as if the body's Matrix links were not in it.

- A body whose only links are Matrix links omits `url_previews` (rule 4), as a body whose only links are `http` links does.
  Sending `[]` would stop other clients from building their own card for it.
  That disclosure was ADR 0132's reason for `[]`, and it does not apply here: what another client could fetch is the matrix.to page, not the message.
- A body with Matrix links and other eligible links follows rules 2 and 3 on the other links alone.
  Kept cards are sent (rule 2), and otherwise `[]` (rule 3), for example when the body's only `https` link was discarded.
  Either list also tells other clients not to preview the Matrix links, which costs nothing, because an Axon recipient's card does not come from the list (§ 6 below).

**Edits** (ADR 0132 § 8).
The edit composer and § 8's rules treat Matrix links as § 6 does, as if they were not in the body: an edit that adds only a Matrix link adds no eligible link, so it does not send `[]` under § 8's rule 3.
A message may already carry a bundled preview for a Matrix link, sent by another client or before this ADR.
Such an entry is left out wherever § 8 reads the message's current previews, as an entry that fails § 8's checks is: the server's carry-forward and `keep` drop it, and edit mode never shows it.
So the first edit removes a stale Matrix-link card, and when it was the message's only entry, carry-forward sends `[]` because the message still has a preview field.
The server tells a Matrix link by the same origin and scheme test as `isMatrixLink`.

### 2. The recipient's client builds the card, from its own access

The web client renders a card under a message for each Matrix link to an event that the message shows as a pill.
It reads the event with the recipient's own account, so a card appears only for a recipient who can read the event, and nothing is copied into the room.
Other Axon users see the same card through their own access; users of other clients see the pill or link their client draws.

- **Which links.** Event links only: a link with an event id, to a room the client can name by id.
  An alias resolves through the account's rooms' canonical aliases, as the pill does; an alias the account does not know gets no card.
  Links to users and to rooms without an event stay pills.
  The links come from the body the message shows, after the reply fallback (`<mx-reply>`) is removed, so a reply from an older client does not get a card for the message it replies to.
- **How many.** The first three distinct events, in text order, as in ADR 0132 § 1.
  A link to the message itself gets no card.
- **When.** A card's event is read only when its message comes within a screen's height of the visible part of the timeline, so scrolling past a long history does not read every linked event in it.
- **No nesting.** A card shows the linked message's text as plain text.
  A Matrix link inside it is text, never another card.
- **A setting of its own.** `matrixLinkCards`, per device like `linkPreviews`, with two values, on and off; the default is on.
  It is separate from `linkPreviews`, which governs fetches that reach other sites or disclose a link before it is sent.
  Off means no cards and no reads, and the links stay pills.
  Any sender in a shared room can make a recipient's client read events through the recipient's homeserver (§ 3), so the recipient gets a way to stop it.

### 3. A room-scoped read that falls back to the homeserver

A new route, `GET /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}`, returns a `LinkedEventDto`.
The path already carries `PUT` (edit) and `DELETE` (redact); this adds `GET`.

1. **The store first.** An event stored for this account and this room is returned from the store, with its latest edit applied and its redaction masked, as the timeline shows it.
   A stored event in a different room is a 404: event ids are unique, so the link is wrong.
2. **Then the homeserver, for a room the account has joined or left.** Only a room whose membership in the account's Matrix client is joined or left is looked up on the homeserver.
   Any other room is a 404 without a request.
   So a sender cannot make a recipient's client probe rooms the recipient has never been in, and a `world_readable` room the account has never joined gets no card.
   An invited room gets no card either, though its history visibility may let the homeserver answer: anyone can invite the account to a room, and a card would show that room's messages before the user has accepted the invite.
   Otherwise Axon asks the account's homeserver for `GET /_matrix/client/v3/rooms/{roomId}/event/{eventId}`, and the homeserver applies the room's history visibility, so it answers only for an account that may read the event.
   Two answers mean there is nothing to show, and are a 404 here: a 404 with `M_NOT_FOUND`, and a 403 with `M_FORBIDDEN`, which is how a homeserver refuses an event the account may not read.
   Every other error, including a 403 or 404 with any other code, or none, is a 502, which the client treats as transient (§ 5), so a refusal from a proxy or a rate limit never leaves a card missing for the session.
3. **Decrypted and judged as the timeline would.** An encrypted event is decrypted by the account's Matrix client, in the same way backfill decrypts a paged event, so any rule the client applies at decryption also applies here, including ADR 0136's exclusion of insecure devices once it is in force.
   When it cannot be decrypted, it is returned with `content: null`, as a UTD.
   A decrypted event carries the `sender_trust` verdict that ingestion would have recorded, computed from the decryption's `EncryptionInfo` by the same function (`meta::sender_trust`); § 5 says what the card does with it.
4. **Redactions and edits.** A homeserver event that carries `unsigned.redacted_because` is returned with `redacted: true` and `content` and `body` null, as a redacted stored event is.
   An edit is applied when the homeserver bundles it in `unsigned["m.relations"]["m.replace"]`, which servers implementing Matrix 1.7 (MSC3925) do with the whole replacement event.
   The replacement is decrypted and judged in the same way, and is used only if it is a valid edit by the original sender under the same rules as stored edits; it then replaces `content` and `body`, sets `edited: true`, and its own `sender_trust` is the one returned.
   Without a bundled replacement, `edited` is false.
5. **Not stored.** An event fetched this way is not written to the store.
   Timeline reads come straight from the store, and an isolated old event in a room still being backfilled, or capped, would appear beyond a gap.
   It also keeps rooms the account has left out of the store and the search index.
   Each fetched event's content is kept in memory for an hour, for its media (§ 4); the event itself is read again in each new session.

`LinkedEventDto` carries only what a card needs: `event_id`, `room_id`, `sender`, `origin_ts`, `type`, `content`, `body`, `relates_to`, `redacted`, `edited`, `sender_trust`, and `source` (`store` or `homeserver`).
`sender_trust` has the values and meaning it has on `EventDto`, and is `null` for an unencrypted event and for a UTD.
It is not an `EventDto`, because a homeserver event has no arrival order or reactions, and a DTO that pretended to have them would invite a client to use them.

The homeserver request runs only because a message is on screen, but no one asked for it, so it is a background caller of the homeserver's load gate (ADR 0122), not an interactive one.
It takes a permit before sending, and waits for one at most five seconds; with no permit by then it answers 503, and the client tries again the next time the message is rendered.
So when the homeserver is struggling, card reads wait behind it rather than add to it, and they report their outcomes to the gate as every other background caller does.
Across the process, at most four run at once, a cap of their own, separate from the 16-slot semaphore media downloads share, so card reads can never take a media download's slot.
SDK retries are off, and one 15-second deadline, starting once the permit is held, bounds the request.
A slow or failed request is an ordinary 502, 503 or 504 that the client treats as transient.
An account that is not connected answers 503, and one that is logged out answers 409, as the other homeserver-backed routes do.

### 4. A card's media is read through the event it names

A card shows a linked image's thumbnail, and a video's poster when the event has one.
The media route serves an `mxc://` object only when a stored event refers to it, so for a fetched event it would 404.
Granting the account that `mxc://` URI by itself would let any event naming the same URI decide how it is served: another event in a room the account can read could name it with a different MIME type or encrypted-file descriptor.

So the card's media request names its event.
The media and thumbnail routes take two optional query parameters, `room_id` and `event_id`, which must be given together.
A request with only one of them is a 400 `bad_request`, never a fallback to today's resolution, which would undo the binding this section exists for.
When they are, the route resolves the `mxc://` URI against that event alone, and serves it only if that event's content names it, in `url`, `file`, `info.thumbnail_url` or `info.thumbnail_file`.
The encrypted-file descriptor and MIME type come from that event's content, exactly as `resolve_media_content` takes them from a stored event today.
The event is the stored one when the account has it, and otherwise the content the linked-event route kept in memory when it fetched it (§ 3).
No other event, stored or fetched, can change how the card's media is served.
Without the parameters, the routes behave exactly as they do today.

Only the fields this needs are kept, the URIs and descriptors named above and their MIME types, and only for an event that names media, so an entry is a few hundred bytes.
The kept contents are bounded per account: 256 events, an hour each, the least recently used going first when the account's table is full.
256 covers what the client can have on screen: the on-screen rule (§ 2) reads the visible messages and one screen's height either side, at most three events each.
One account's reads can therefore never evict another's, and an eviction costs a homeserver read, not a broken card.

A media request for an evicted or expired event is a 404, and the card then retries once:

- It reads the event again, bypassing the session cache of § 5, and the new result replaces the cached one.
  Concurrent retries for one event share one such read.
  The read keeps the event's content again.
- If the read succeeds, the card requests the image once more.
  If that also fails, the card stays without its image for the rest of the session, with no further retries, so an image can neither loop nor flicker.
- If the read answers 404, the event is no longer readable, and the card goes, as for any 404.
  A transient failure leaves the card without its image until it is rendered again.

The same happens after a restart.

### 5. The card

- It shows the sender's name, the room's name, the time, and up to three lines of the message's text.
  A name the client does not know is shown as the Matrix id.
- An image or sticker shows its thumbnail, and a video its poster, through the media request of § 4.
  A file or audio message shows its name.
- A redacted event reads "Message deleted"; an undecryptable one reads "Message could not be decrypted".
  Any other event type gets a one-line description, as the timeline gives it.
- **Sender trust.** The card is shown away from the room the message came from, so it says plainly when its `sender_trust` does not support the sender's name:
  - `verification_violation`: "The sender's identity has changed since you verified it."
  - `unknown`: "Sent from a device that can no longer be identified."
  - `mismatched_sender`: "Encrypted by another user's device, such as a bridge."
    This is how every message in an encrypted bridged room arrives, so the text is shown, not hidden as a UTD would be, and the card names the sender as the event does.
  - `verified`, `unverified`, and `null` add nothing.
    `unverified` is the common case for a sender this account never verified, and the timeline does not mark it either.
  The timeline shows the same verdicts in the event inspector and nowhere else today; a timeline badge, when one is added, should use the same words.
- The card is a link to the event in its room, like the pill: in its thread when the event is a thread reply, and to the room's join flow when the account has not joined it.
- A read that answers 404 leaves no card.
  A transient failure leaves no card either, and the read is tried again when the message is rendered again.
- Results are cached in memory for the session, keyed by account, room and event, and concurrent reads of one event share one request.
  A 404 is cached; a transient failure is not.
  Only the media retry of § 4 reads past the cache.

### 6. Received bundled previews of Matrix links are not shown

Another client may send a bundled preview for a matrix.to link.
Its contents are the matrix.to page's, which the context above explains is useless, or the sender's own claim about the message.
The renderer skips any received preview whose `matched_url` is a Matrix link, whether or not this client can show its own card for it.

## Consequences

- A Matrix link never sends the sender's view of a message to the room, so no message leaves its room or its encryption through a card.
- Only Axon users see the card.
  Recipients on other clients see whatever their client draws for the link, which is usually a pill.
- Any sender in a shared room can make a recipient's client read up to three linked events per message, in rooms the recipient has joined or left.
  The setting turns that off; otherwise the client's cache, the on-screen rule, the three-card cap, the server's cap of four, and the load gate bound it.
- The homeserver learns which linked events the account's client read, and when, including events another user chose to link.
  It already serves that account's rooms and sees the account's own reads of them, but this tells it what was on the user's screen, not only what the user scrolled to.
- A store event and a homeserver event of the same link can differ only where the homeserver omits a bundled edit; otherwise both show the latest valid edit, and both mask a redaction.
- A card can lag the event: an edit or redaction after the read is not shown until the message is rendered again in a new session.
- The fetched contents live in memory, so a multi-process deployment would need them shared; Axon runs one server process.
- Media requests without `room_id` and `event_id` keep today's resolution, which picks one stored event among those naming a URI.
  This ADR narrows that only for cards.
- The TUI gets neither change for now; `docs/client-parity.md` records the gap.

## Delivery

1. This ADR.
2. **Server:** the room-scoped event route and `LinkedEventDto`, the homeserver read behind the gate, the kept contents, the media routes' `room_id` and `event_id` parameters, and dropping Matrix-link entries from an edit's carry-forward and `keep` (§ 1).
   The OpenAPI document (`openapi/openapi.json`) is regenerated with the new operation, parameters and schema, and `clients/web/src/api/schema.d.ts` with it, as the generated-artifact exception allows.
3. **Web:** sections 1, 2, 5 and 6, including the edit composer's part of § 1, and the `matrixLinkCards` setting, stacked on the server PR, with `docs/client-parity.md` and `docs/demo-coverage.md` updated.
