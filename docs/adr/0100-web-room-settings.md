# ADR 0100 — Web room settings (name, topic, avatar)

**Status:** Accepted — implements ADR 0069's **M19-W6** against the M19d
contracts that landed with ADR 0068.

## Context

ADR 0068's M19d shipped `PUT .../rooms/{room_id}/{name,topic}`,
`PUT/DELETE .../rooms/{room_id}/avatar`, and `PUT/DELETE .../tags/{tag}`.
ADR 0069 then scoped the client rollout and named M19-W6 as "room settings for
name, topic, and avatar, including staged-avatar upload integration".

Neither client ever consumed any of it. `docs/client-parity.md` carried the
row as server-Done / both-clients-Not-started, so a web user could read a
room's name, topic and avatar in the Room Information panel but had to open
Element to change them.

ADR 0069 already fixed the wire-level decisions (empty string clears
name/topic; avatars go through the existing staged-upload flow rather than a
hand-typed `mxc://`; no optimistic state as the source of truth). This ADR
records only what it left open.

## Decision

### Editing lives in the Room Information panel

An **Edit** button sits left of **Close** in the panel header and swaps the
identity block — and the Name/Topic/Avatar rows of the Details list — for a
form. One surface, no new route, and the read-only rows are hidden while the
form owns those fields so no stale value sits beside an input.

Only genuinely changed fields are written. The three routes are independent,
so a save issues one request per dirty field and reports partial failure as
partial ("Saved name. Could not save topic: …") rather than as success.

### The avatar is viewable, and replaceable from that view

Clicking the identity avatar opens it full size in the existing `Lightbox`
(ADR 0064/0072) via `LightboxImage`, which already fetches the full object
rather than a thumbnail and reports whether the bytes decoded. ADR 0101 later
widened that component from an `mxc` URL to a full `ParsedMedia` descriptor;
an avatar has no event behind it, so this call site synthesises one — the
mimetype is genuinely unknown, since `m.room.avatar` carries `info.mimetype`
only when whoever set it included one, and this panel reads the room summary
rather than the state event. Only a real
image is a control: with no `m.room.avatar` the fallback is a coloured
letter, and a viewer over that shows nothing. The room-list avatars are
deliberately left alone — a click there opens the room, and hijacking it
would break navigation.

What the form edits is `room.avatar_url`, deliberately **not** the effective
display avatar. `roomListAvatarUrl` falls back to the DM peer's profile
picture, which is not this room's `m.room.avatar`: treating it as one made the
form offer "Remove avatar" for a room that has none, where DELETE clears
nothing and the peer's picture stays on screen. The identity block still shows
the fallback, as the room list does, and the editor says so when the two
differ — otherwise the avatar appears to vanish on opening the editor.

**A replace from the viewer hands the file to the edit form rather than
writing it.** The alternative — writing immediately from the viewer — would be
a second write path with no preview and no undo, for a change every member of
the room can see. Routing it through the form instead means one save path, one
set of validations, and the new image shown as a preview awaiting Save. The
control is offered on the same terms as Edit — see the power-level section
below, which is why it is not hidden when the thresholds look unfavourable.

The control is a `<label>` wrapping a file input, not a button calling
`.click()`: browsers require a real user gesture to open a file dialog, and a
synthetic click from an effect loses it. That in turn needed `label` and
`input` added to the lightbox's `DISMISS_EXEMPT`, or the overlay would dismiss
itself out from under the file dialog it had just opened.

### The power-level read is a caution, never a gate

The panel reads `GET .../rooms/{room_id}/power_levels` alongside its existing
`/info`, `/pinned`, `/space/*` and `/upgrade` fan-out. `m.room.name`,
`m.room.topic` and `m.room.avatar` are all state events with no threshold of
their own, so `state_default` covers all three.

**Editing is never disabled on it.** The read is a hint used only for wording,
and it is known to be wrong in both directions:

- From **room version 12** a room's creators hold an effectively infinite
  power level and cannot appear in `users` at all, so a creator resolves to
  `users_default` — normally 0. `ruma` models this
  (`RoomPowerLevels::for_user` returns `Infinite` for a privileged creator)
  but `PowerLevelsDto` flattens the levels and drops the creator set, so a
  client cannot reconstruct it. An earlier version of this feature disabled
  **Edit** on that reading and locked room owners out of their own rooms
  (#324).
- `PowerLevelsDto` carries no `events` map either, so a room overriding one of
  these event types specifically — rare, but spec-legal — is invisible.

The failure is asymmetric, and that settles it: showing the form to someone
who cannot save costs them one clear 403, while blocking someone who can costs
them the feature outright. So the caution is worded as a likelihood — "your
power level looks like 0 … the server decides" — and both **Edit** and the
viewer's replace control stay available whatever the numbers say.

Verified against a live v12 room: `users` comes back `{}`, and a write by a
joined non-creator returns `403 M_FORBIDDEN user_level (0) < send_level (50)`.
That room's _creator_ receives an identical payload but a successful write, so
the two cases are indistinguishable over this API. #324 asks the server for
the caller's own resolved level, which would make the caution exact.

### Live state events patch the room list

`rooms.noteTimelineEvent` previously ignored every non-content event, so a
rename — from Element, or from this new form — did not reach the room list
until the socket reconnected. It now patches `name`/`topic`/`avatar_url` in
place from `m.room.name`/`m.room.topic`/`m.room.avatar` frames, without
touching `last_activity_ts` (a settings change is not "recent activity").

A successful save also fires a background `rooms.refresh()` as the
socket-down fallback. Neither path is optimistic: both apply server truth.

The existing `applyLocalRoomMetadata` overlay was **not** reused. It is
fill-only — it substitutes a value only where the server's field is blank — so
it can express a newly created room's name but never a rename.

### Wire shapes were verified, not assumed

`EventDto.content` is raw Matrix content passed straight through, so the patch
above depends on shapes a mock cannot validate: writing the fixture and the
reader from the same assumption tests nothing. Captured from `/v1/ws` against a
live homeserver:

| Event           | Set                               | Cleared         |
| --------------- | --------------------------------- | --------------- |
| `m.room.name`   | `{"name": "x"}`                   | `{"name": ""}`  |
| `m.room.topic`  | `{"topic": "x", "m.topic": {…}}`  | `{"topic": ""}` |
| `m.room.avatar` | `{"url": "mxc://…", "info": {…}}` | `{"url": null}` |

`topic` is read rather than the richer `m.topic` block beside it: the spec
requires it to duplicate that block's plain text, and `RoomDto.topic` is plain
text. The cleared-avatar shape matches ruma's `RoomAvatarEventContent`, whose
`url` field has no `skip_serializing_if`, so `remove_avatar()` sends the key
explicitly as `null`.

The patch also requires `state_key === ""`. These three are singleton state,
and a same-type event under another state key is not canonical — anyone able
to send state could otherwise rename or re-avatar the room in every client
that trusted the type alone. Axon was observed sending `""` for singleton
state and the user id for `m.room.member`, so the check matches the server's
own projections rather than assuming a shape.

The reader still treats a **missing** key as unset rather than unchanged: a
state event's content is the complete new state, and another client may clear
an avatar with `{}`.

### Avatar validation happens before the network

A picked file is refused client-side unless `file.type` starts with `image/`,
and above a modest ceiling (8 MiB — `MAX_UPLOAD_BYTES` is 100 MiB, sized for
arbitrary media sends, not for an image every member downloads and the UI
renders at ~56px). Both refusals mirror a real server `400` observed live —
"image uploads must have an image/\* content type" and "avatar upload must
declare a content type" — so this turns two round trips into an immediate
message. A file with no detectable type fails the same check, which is what
makes the second case unreachable from this client. Bytes are uploaded
unresized, matching media-send.

**Neither check looks at the bytes, so a third one decodes them.** `file.type`
is derived from the file's _extension_: a text file renamed `holiday.jpg`
arrives as a perfectly valid `image/jpeg` and clears every cheap check —
including the server's, verified against a live Axon by staging a 30-byte
ASCII file as `image/jpeg` and having it accepted. Left there, the avatar is
set to bytes no client can render and every member sees a broken image with
nothing having reported an error. The client therefore loads the picked file
into an `Image` and requires it to decode with non-zero dimensions before the
file is staged at all. Decoding covers every format the browser can display,
so there is no per-format magic-number table to keep in step with whatever
`accept="image/*"` lets through.

An avatar can also be **dropped on the form or pasted into it**, reusing the
existing `useFileDrop` hook (ADR 0065) and its `.drop-overlay` — the same
affordance the composer already has for media sends, so the gesture behaves
the way it does elsewhere in the app. All three sources funnel through one
`acceptFile`, which is what stops a drop quietly skipping the decode check the
picker performs. A paste carrying no image is left uncancelled so it reaches
the name or topic field the user was typing in.

The picked image is shown immediately via an object URL (`RoomAvatar` gained a
`previewUrl` prop) until the real `mxc://` arrives. Revocation is tied to the
URL's own lifetime in an effect, so it survives the panel dropping the form
without going through Cancel.

### Concurrency: what an open editor may and may not write

Three rules, each from a way the naive version got it wrong.

**Dirtiness is measured against a baseline captured when the editor opened**,
never against the live `room` prop. That prop keeps updating while the editor
is open — a rename from another client arrives over the socket and is patched
in. Compared against the live value, a field the user never touched turns
"dirty" the moment someone else changes it, and Save writes the stale value
back, silently reverting them.

**A save is scoped to the form that started it.** It spans several awaits, and
the panel keeps one form per room; a save begun for room A that lands after the
user has moved to room B must not call back into the panel and clear B's draft.
The form tracks its own liveness and drops only the UI update — the requests
are A's and are allowed to finish.

**Discard is not offered once a save is in flight.** The requests are already
out, so the changes are not unsaved; a "Discard changes?" prompt would be
describing something that has already happened, and closing does not cancel
them.

**Every exit from the panel goes through the same guard**, not just Close.
Starting a DM with a member and opening a related room both navigate away and
_then_ close, so the check has to run before the action rather than at the
`onClose` it ends with — and confirming resumes the interrupted action rather
than merely closing. A guarantee that covers only one of three exits is not a
guarantee.

**Finishing a save is not conditional on the form still existing.** Clearing
the parent's saving flag and firing the socket-down fallback refresh both run
even after an unmount: the flag left stuck would disarm the discard guard for
whatever room the user moved on to, and the refresh exists precisely for the
case where nothing else will reflect the write. Only the local UI update is
dropped.

**Save errors stay out of the shared `error` signal.** That signal drives the
app-wide banner; a settings failure is reported inline by the form that issued
it. Writing there would duplicate the message and — worse — _clearing_ there
would erase somebody else's: a topic PUT succeeding would wipe the banner for
the name PUT that failed in the same save.

### A refresh must not undo a newer live frame

`setRoomAvatar` returns before the homeserver has told Axon about the new
avatar — measured at ~400-600ms against a live server — so the fallback
`rooms.refresh()` fired on save reads a room that still has the old one. The
`m.room.avatar` frame lands meanwhile and patches it in, and then the older
response arrives and, replacing the list wholesale, erases it. The upload
appears to have done nothing.

`doRefresh` therefore records the live-patch counter when it starts and keeps
any _field_ patched after that point — per field, not per room. A live
`m.room.name` frame says nothing about the topic, and the same response may
legitimately carry a newer topic set by another client that has not reached
this socket yet; reverting all three together would silently undo it. Only those three
fields: everything else on the row is the server's to state, and a stale value
there self-corrects on the next refresh rather than looking like a write that
did not happen.

## Consequences

- **Pro:** the first client consumer of M19d; `docs/client-parity.md`'s
  longest-standing server-only row moves for `axon-web`.
- **Pro:** the live-frame fix repairs a pre-existing gap — a rename from any
  other client now reaches the room list during a session, not just on
  reconnect.
- **Con / accepted:** the edit caution can disagree with the homeserver — for
  a room-version-12 creator it is simply wrong, and for a room with per-event
  overrides it can err either way (#324). Nothing is blocked on it, and the
  403 path is implemented and tested rather than assumed away, so the cost is
  a misleading hint rather than a lost capability.
- **Con / accepted:** a remote rename is not written to the IndexedDB
  room-list cache — `cache.write` runs only at the end of `doRefresh` — so a
  cold start can show a stale cached name until the first refresh lands. This
  matches how live previews and unread counts already behave; changing the
  cache-write cadence for one field is not worth it.

## Known follow-ups

- **#343** — `roomMetadataPatch` reimplements the server's own
  event-to-display-field projection, a third copy that can drift. The
  suggested direction is a server-pushed resolved metadata frame so clients
  apply values instead of re-deriving them.
- **#344** — the three field writes are issued sequentially rather than
  concurrently, costing ~3x round-trip latency on a full edit. Sequential
  ordering is what makes the partial-failure report deterministic, so the
  change needs `Promise.allSettled` plus a fixed reporting order.

## Out of scope

- **Tags (M19-W7) are blocked, not deferred by preference.** `PUT/DELETE
.../tags/{tag}` exists, but there is no read: `RoomDto` has no `tags` field
  and the API exposes no room account data at all. A favourite toggle could be
  written and never truthfully displayed, so it is not built. Tracked as a
  server gap (see `docs/client-parity.md`). The existing web "Favorites"
  filter is `settings.pinnedRooms` in `localStorage` — purely local, unrelated
  to `m.tag`, and untouched.
- **The power-levels editor (M19-W8)** — this PR reads levels and never writes
  them. The self-demotion guardrail and `acknowledge_self_demotion` belong
  there.
- **Room settings for TUI**, a separate silo and a separate PR.

## Testing note

`@testing-library/preact` re-maps `fireEvent.change` to React's `onChange`
semantics (an `input` event). A file input never fires `input`, so the picker
handler would silently never run and an assertion would pass or fail for the
wrong reason. The panel tests dispatch a real `change` through the generic
`fireEvent(el, event)`, which keeps the `act()` wrapper. The Playwright lane
covers the part jsdom cannot: real bytes through a real picker, the rename
round-tripping back through `/v1/rooms`, and — the reason that lane is
load-bearing rather than a nicety — a genuinely corrupt `.jpg` being refused
by a real decoder, plus a real `DataTransfer` drop (the unit tests hand-build
that object). The drop test also asserts the composer staged _nothing_: the
composer owns its own drop target on `.room-stream`, and the panel is a
sibling of that pane rather than a child, so ADR 0065's per-pane scoping is
what keeps an avatar drop from also becoming a message attachment. That is
worth pinning, since a later layout change could nest the two.

Each of these tests restores the mock's room settings before finishing. The
e2e mock server is one process shared by all three browser projects, which run
in sequence, so state left behind by the last test of one engine is the
starting state of the first test of the next. jsdom loads no resources, so an `Image` there fires
neither `load` nor `error`; the unit tests stub it, which means only the
browser lane actually exercises the check.

That lane asserts on all three fields in one poll rather than checking the
avatar after a name/topic poll. The avatar is written last — a staged upload,
then the claim — so the narrower poll passed on Chromium while the avatar
request was still in flight, and failed on Firefox. Engine timing was the only
thing separating a green run from a red one.
