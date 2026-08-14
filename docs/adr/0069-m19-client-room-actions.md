# ADR 0069 — Client rollout: M19 room and account actions

**Status:** Proposed — client-side companion to ADR 0068 (M19). Tracked in
issue #304.

## Context

ADR 0068 designed the server-side Matrix C-S verb batch. When the first draft
of this ADR was written, only M19b (membership verbs) and M19c (room entry)
had landed, so it deliberately blocked public-room discovery on M19f and left
room settings/power levels out of the client plan.

That is no longer the repository state. On `main`, **all M19 server batches
have landed: M19a through M19f**. `docs/client-parity.md` now tracks five
server-done/client-not-started rows that this ADR must cover:

- Room membership: leave/forget/invite/kick/ban/unban (M19b).
- Room entry: join/knock/create-room/create-DM (M19c).
- Room settings: name/topic/avatar/tags (M19d).
- Power levels: read/write with self-demotion guardrail (M19e).
- Account actions: profile, ignore/unignore, user-profile read, public-room
  directory search (M19f).

M19a typing notices are already surfaced by both clients and are not part of
this follow-up rollout. ADR 0067's outbound read receipts are likewise already
surfaced by both clients.

Tier C of issue #279 (invited-room visibility, presence, unread counts) remains
separate from M19. Invited-room visibility has since landed as ADR 0091
(`GET /v1/invites` plus `invite.added` / `invite.removed`); it is a dedicated
projection, not an extension of `list_rooms`. Presence remains deferred per
ADR 0056. Unread counts landed separately as ADR 0070.

## Decision

Client work proceeds **one silo per PR** (`clients/tui/` or `clients/web/`,
per AGENTS.md). Each PR must use the implemented M19 wire contracts on `main`,
not ADR 0068's earlier design-time placeholders. The client rollout covers
nine user-facing features:

1. **Leave / forget** a room (M19b).
2. **Invite / kick / ban / unban** room members, with cached-data username
   autocomplete (M19b).
3. **Join / knock** a room, including `matrix.to`/`matrix:` links (M19c).
4. **Create a room / start a DM** (M19c).
5. **Public-room discovery** across homeservers, with join from a result
   (M19f).
6. **Room settings** for name, topic, and avatar (M19d).
7. **Room tags** for favorites/low-priority/custom tags (M19d).
8. **Power levels** viewer/editor with self-demotion confirmation (M19e).
9. **Account and user actions**: own display name/avatar, user-profile read,
   ignore/unignore (M19f).

The **invitation inbox** is now unblocked on the server (ADR 0091). Accepting
an invite is still just join, and rejecting is still leave, against room ids
from `GET /v1/invites`. Client inbox UI is a separate silo follow-up.

## Implemented Server Contracts

Verified against `crates/axon-api/src/routes/*`, `crates/axon-api/src/dto.rs`,
`openapi/openapi.json`, and `docs/client-parity.md` on `main`.

- **Membership verbs (M19b)** return an empty object, not an event id:
  `POST /v1/accounts/{account_id}/rooms/{room_id}/{leave,forget,invite,kick,ban,unban}`
  all respond `200 {"data": {}}`. `leave`/`forget` take no body; `invite`
  takes `{user_id}`; `kick`/`ban`/`unban` take `{user_id, reason?}`. The
  resulting `m.room.member` state event round-trips through ordinary sync, so
  clients confirm by refreshing room/member state.
- **Room-entry verbs (M19c)** return `{room_id}` only:
  `POST /v1/accounts/{account_id}/rooms/join`, `/rooms/knock`, `/rooms/dm`,
  and `POST /v1/accounts/{account_id}/rooms`. Join/knock bodies use
  `{room_id_or_alias, server_names?}` plus optional knock `reason`; the wire
  field is `server_names`, not `via`. `create_dm` takes `{user_id}`.
  `create_room` accepts optional `name`, `topic`, `invite`, `is_direct`,
  `public`, `preset`, and `encrypted`; an empty body creates a private,
  unencrypted, unnamed room. `create_room` and `create_dm` are not idempotent,
  so clients must avoid automatic retry loops that can create duplicates.
- **Room settings (M19d)** return an empty object. `PUT .../rooms/{room_id}/name`
  takes `{name}` and `PUT .../topic` takes `{topic}`; empty strings clear those
  fields. `PUT .../avatar` takes `{upload_id}` for an already-staged image
  upload, and `DELETE .../avatar` clears it. These write room state.
- **Room tags (M19d)** return an empty object and write private room account
  data, not state. `PUT`/`DELETE .../rooms/{room_id}/tags/{tag}` supports
  `m.favourite`, `m.lowpriority`, `m.server_notice`, and `u.`-prefixed custom
  tags. `PUT` takes `{order?}` with order in `[0, 1]`.
- **Power levels (M19e)** mix a read and a mutation. `GET .../power_levels`
  returns fully resolved role thresholds plus the per-user map. `PUT` takes
  optional `ban`, `invite`, `kick`, `redact`, `events_default`,
  `state_default`, `users_default`, a `users` map, and
  `acknowledge_self_demotion`. A write that would strand the caller below the
  level needed to send a future `m.room.power_levels` event is rejected unless
  `acknowledge_self_demotion` is set.
- **Account actions (M19f)** return an empty object for mutations and data for
  reads. `PUT .../profile/display_name` takes `{display_name}`; an empty value
  clears it. `PUT .../profile/avatar` takes `{upload_id}` for an already-staged
  image upload; `DELETE .../profile/avatar` clears it. `GET
.../users/{user_id}/profile` returns `{user_id, display_name?, avatar_url?}`.
  `PUT`/`DELETE .../users/{user_id}/ignore` ignore/unignore a Matrix user.
  `GET .../directory/public_rooms` accepts optional `server`, `search_term`,
  `limit`, and `since`, and returns a paginated room directory page.

Because most mutation responses carry no event id, both clients should use
their existing refresh/reconcile paths after successful writes: TUI
`refresh_rooms`, web `rooms.refresh()`, and targeted member/settings refetches
where a panel already owns fresher state. Do not invent optimistic state as the
source of truth for membership, room settings, tags, or power levels; optimistic
UI is acceptable only as a pending affordance until the authoritative refresh
lands.

## Client Product Decisions

- **Web registers an OS-level `matrix:` protocol handler** via
  `navigator.registerProtocolHandler`, in addition to in-app interception of
  `matrix.to`/`matrix:` links in message bodies and a `/join` command. This is
  feature-detected and gated behind a one-time settings opt-in so it never
  triggers an unprompted browser permission dialog.
- **`/part` is a synonym for `/leave`** in both clients, so command muscle
  memory carries over.
- **Matrix URI parsing** maps a `matrix.to`/`matrix:` link to
  `{target, server_names}`. TUI wraps `ruma`'s `MatrixToUri`/`MatrixUri`; web
  hand-writes an equivalent parser because `ruma` is not in the JS toolchain.
- **Invite autocomplete draws only from already-cached in-memory data**:
  members of the account's other joined rooms plus recent timeline senders,
  unioned and deduplicated client-side. This adds no new network fan-out.
  Free-form `@user:server` entry remains available for users the account has
  never shared a room with.
- **Avatar changes reuse media staging** rather than accepting direct `mxc://`
  entry. The client flow is: stage image bytes with the existing upload route,
  then pass the returned `upload_id` to the room/account avatar route.
- **Power-level editing must be conservative by default.** A client should show
  the resolved current values first, write a merged change, and require an
  explicit confirmation before setting `acknowledge_self_demotion`.

## Consequences

- **Pro:** all M19-backed client features are unblocked on the server today;
  public-room discovery is no longer waiting on a future server PR.
- **Pro:** capturing the implemented contracts once prevents each client PR
  from re-discovering empty-object responses, staged-avatar upload flow, tag
  validation, and power-level self-demotion behavior independently.
- **Pro:** the plan now aligns with `docs/client-parity.md`, so server/client
  gaps are tracked in one place while design rationale stays here.
- **Con / accepted:** the rollout is larger than the original room-actions-only
  draft. Splitting by feature and by client silo is more PRs, but keeps review
  surface focused and preserves AGENTS.md's one-silo-per-PR rule.
- **Con / accepted:** invitation inbox UI is still a client follow-up. The
  server projection it needed is ADR 0091, not this ADR.

## Suggested PR Sequence

All non-invite-inbox items are **ready now** because M19a-M19f have landed.
Each item below means one TUI PR and one web PR unless a client-specific reason
requires splitting further:

1. **M19-W1:** Leave/forget plus `/part` (Feature 1).
2. **M19-W2:** Join/knock, Matrix URI parsing, message-link interception, and web's opt-in
   `matrix:` protocol handler (Feature 3).
3. **M19-W3:** Public-room directory search with join-from-result (Feature 5).
4. **M19-W4:** Create-room and create-DM flows (Feature 4).
5. **M19-W5:** Invite/kick/ban/unban, including cached-data user autocomplete (Feature 2).
6. **M19-W6:** Room settings for name/topic/avatar, including staged-avatar upload
   integration (Feature 6).
7. **M19-W7:** Room tags and favorites/low-priority UI backed by `m.tag` (Feature 7).
8. **M19-W8:** Power-level viewer/editor with self-demotion confirmation (Feature 8).
9. **M19-W9:** Account profile, user profile read, and ignore/unignore UI (Feature 9).

**Ready:** invitation inbox PRs for web (and later TUI) can consume ADR 0091's
`GET /v1/invites` plus join/leave. TUI is not required to grow an inbox.

Each PR updates the corresponding `docs/client-parity.md` row in the same PR,
per that doc's cross-silo exception to the one-silo-per-PR rule.
