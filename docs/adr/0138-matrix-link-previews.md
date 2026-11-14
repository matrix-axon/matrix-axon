# ADR 0138 — Previews of Matrix links, built by the recipient

**Status:** Proposed. Amends [ADR 0132](0132-composer-link-previews.md) § 1; supersedes nothing.
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
  The exceptions are a room still being backfilled, an operator's depth cap or disabled backfill, and rooms the account has not joined but can read, such as a `world_readable` room or one it has left.
- **Media is authorized by the event that names it.**
  `GET /v1/media/{account_id}/{server_name}/{media_id}` serves an `mxc://` object only when a stored event refers to it, and takes the encrypted-file descriptor and MIME type from that event (`resolve_media_content` in `routes/media.rs`).

## Decision

### 1. The sender never builds a card for a Matrix link

A Matrix link is any link whose origin is `https://matrix.to`, and any `matrix:` URI, whatever it points at: a user, a room or an event.
The composer gives it no card and no lookup, on any platform, and never offers "Try homeserver" for it.

The rule sits beside ADR 0132's static check, not inside it.
`isPreviewLink` stays the network boundary that the shell's plugin and the Swift reader mirror rule for rule through their shared fixture.
The places that choose which links get cards apply it together with a new `isMatrixLink` (in `matrix-to.ts`), and `nativePreviewLinks` does the same.

So a Matrix link is not an eligible link in ADR 0132 § 6.
A message whose only links are Matrix links omits `url_previews`, as a message whose only links are `http` links does.
Sending `[]` would stop other clients from building their own card for it.
That disclosure was ADR 0132's reason for `[]`, and it does not apply here: what another client could fetch is the matrix.to page, not the message.

Share recovery checks a staged card against `nativePreviewLinks`, so a card the iOS Share Extension built for a matrix.to link is dropped before it is sent.
The Share Extension's Swift reader still fetches the page; teaching it the same rule is iOS work, left for later.

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
- **No nesting.** A card shows the linked message's text as plain text.
  A Matrix link inside it is text, never another card.
- **No setting.** The `linkPreviews` setting governs fetches that reach other sites or disclose a link to the homeserver.
  This lookup goes to the recipient's own Axon server, and from there to the homeserver the recipient's account already uses for that room, so it needs no setting.

### 3. A room-scoped read that falls back to the homeserver

A new route, `GET /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}`, returns a `LinkedEventDto`.
The path already carries `PUT` (edit) and `DELETE` (redact); this adds `GET`.

1. **The store first.** An event stored for this account and this room is returned from the store, with its latest edit applied and its redaction masked, as the timeline shows it.
   A stored event in a different room is a 404: event ids are unique, so the link is wrong.
2. **Then the homeserver.** Otherwise Axon asks the account's homeserver for `GET /_matrix/client/v3/rooms/{roomId}/event/{eventId}`.
   The homeserver applies the room's history visibility, so it answers only for an account that may read the event.
   An encrypted event is decrypted with the room keys Axon holds, in the same way backfill decrypts a paged event; when it cannot be, the event is returned with `content: null`, as a UTD.
   A homeserver 403 or 404 is a 404 here, so the route does not reveal which rooms exist.
3. **Not stored.** An event fetched this way is not written to the store.
   Timeline reads come straight from the store, and an isolated old event in a room still being backfilled, or capped, would appear beyond a gap.
   It also keeps rooms the account has not joined out of the store and the search index.

`LinkedEventDto` carries only what a card needs: `event_id`, `room_id`, `sender`, `origin_ts`, `type`, `content`, `body`, `relates_to`, `redacted`, `edited`, and `source` (`store` or `homeserver`).
It is not an `EventDto`, because a homeserver event has no arrival order, reactions or recorded sender trust, and a DTO that pretended to have them would invite a client to use them.
A homeserver event's edits are not applied: the card shows the event as the homeserver returned it.

The homeserver request is made because a user is looking at a message, as a media download is, so ADR 0122 counts it as interactive.
It never waits for the gate, and reports a timeout or server error to it through `HomeserverGates::interactive_call`.
Like media downloads, it has its own cap on concurrent requests across the process, here four, and SDK retries are off.
It is bounded by one 15-second deadline, and a slow or failed request is an ordinary 502, 503 or 504 that the client treats as transient.
An account that is not connected answers 503, and one that is logged out answers 409, as the other homeserver-backed routes do.

### 4. Media of a fetched event is authorized for a short time

A card shows a linked image's thumbnail, and a video's poster when the event has one.
For a stored event the media route already works.
For a fetched event it would 404, because no stored event names the `mxc://` URI.

So when the route returns a homeserver event whose content names media (`url`, `file`, `info.thumbnail_url` or `info.thumbnail_file`), it records a grant in memory: account, `mxc://` URI, and that event's content.
`resolve_media_content` consults the grants after the store, and takes the encrypted-file descriptor and MIME type from the granted content exactly as it does from a stored event.
A grant lasts an hour and the table is bounded, the oldest grant going first.
After a restart or an expiry, the next read of the event grants it again, so a card that is rendered again works.

A grant lets the account fetch only media that an event the homeserver just returned to that account refers to, which is the same rule the store enforces for stored events.

### 5. The card

- It shows the sender's name, the room's name, the time, and up to three lines of the message's text.
  A name the client does not know is shown as the Matrix id.
- An image or sticker shows its thumbnail, and a video its poster, through the existing media path.
  A file or audio message shows its name.
- A redacted event reads "Message deleted"; an undecryptable one reads "Message could not be decrypted".
  Any other event type gets a one-line description, as the timeline gives it.
- The card is a link to the event in its room, like the pill: in its thread when the event is a thread reply, and to the room's join flow when the account has not joined it.
- A read that answers 404 leaves no card.
  A transient failure leaves no card either, and the read is tried again when the message is rendered again.
- Results are cached in memory for the session, keyed by account, room and event, and concurrent reads of one event share one request.
  A 404 is cached; a transient failure is not.

### 6. Received bundled previews of Matrix links are not shown

Another client may send a bundled preview for a matrix.to link.
Its contents are the matrix.to page's, which the context above explains is useless, or the sender's own claim about the message.
The renderer skips any received preview whose `matched_url` is a Matrix link, whether or not this client can show its own card for it.

## Consequences

- A Matrix link never sends the sender's view of a message to the room, so no message leaves its room or its encryption through a card.
- Only Axon users see the card.
  Recipients on other clients see whatever their client draws for the link, which is usually a pill.
- Rendering a message with an event link can make one homeserver request per linked event the store lacks.
  The client's cache, the three-card cap and the server's concurrency cap bound it, and its failures still slow the gate's background callers.
- The homeserver learns that the account read the event.
  It already serves that account's room, so this discloses nothing to a new party.
- A card can lag the event: an edit or redaction after the read is not shown until the message is rendered again in a new session.
- Media grants live in memory, so a multi-process deployment would need them shared; Axon runs one server process.
- The TUI gets neither change for now; `docs/client-parity.md` records the gap.

## Delivery

1. This ADR.
2. **Server:** the room-scoped event route and `LinkedEventDto`, the homeserver read, and the media grants.
3. **Web:** sections 1, 2, 5 and 6, stacked on the server PR, with `docs/client-parity.md` and `docs/demo-coverage.md` updated.
