# Client Feature Parity

A human-centric, cross-silo tracker for one recurring problem: Axon ships a
server capability, one client adopts it, and there is no single place that
says whether the _other_ clients ever caught up. This surfaced concretely in
review of issue #279 (Adam's comment): TUI had no tracking against web once
web existed, web now reads ephemeral indicators (typing, receipts) that TUI
still doesn't, and interactive SAS device verification shipped a full UI in
TUI but never got one in web.

This doc is **not** auto-generated and does not replace `AGENTS.md`'s
"Current state" (server-side landing history) or the ADR log (design
decisions). It answers one question per row: _for a capability the server
already exposes, which clients actually surface it to a user?_

## How to maintain this

- Update the row for a capability in the **same PR** that changes its status
  in any silo (`crates/`, `clients/tui/`, `clients/web/`) — per AGENTS.md's
  `docs/` cross-silo exception, this file can land alongside a change in any
  one silo without violating the one-silo-per-PR rule.
- If you don't know a cell's true status, write "needs confirmation" and say
  why, rather than guess. A wrong "Done" is worse than a known gap — the
  whole point of this doc is to stop parity drift from going unnoticed.
- Add a row as soon as a new server capability is designed (even before it
  lands), so the client-consumer gap is visible from day one instead of
  discovered later.

**Legend:** Done · Gap (server has it, this client doesn't) · Planned
(designed, not landed) · Not started · Deferred (deliberately not building
yet)

## Matrix

<!-- prettier-ignore -->
| Capability | Server (`/v1/`) | axon-tui | axon-web | iOS (future) | Notes |
|---|---|---|---|---|---|
| Text send / edit / redact / react | Done (M6, ADR 0021) | Done | Done | Not started | |
| Media send (`m.image`/`m.file`) | Done (M15, ADR 0059) | Done | Done | Not started | |
| Media read proxy + LRU cache | Done (M11, ADR 0045) | Done | Done | Not started | |
| Inline media preview (audio/video/pdf/text) | n/a — client-side rendering over the existing proxy | **Gap** — download only (a terminal cannot play media) | Done (ADR 0072) — click-to-expand player below the attachment card | Not started | Blob-backed with per-kind size ceilings; `Range` streaming deferred to a media service worker |
| Syntax highlighting (code blocks + text attachments) | n/a — client-side rendering | **Gap** — plain text | Done (ADR 0073) — highlight.js core + per-language lazy chunks; language from `language-*` class, extension, or shebang | Not started | The sanitizer has preserved `language-*` classes since ADR 0046; nothing consumed them until now |
| Image gallery rows (adjacent images from one sender) | n/a — inferred client-side; Matrix has no album and Axon cannot carry a marker (#130) | **Gap** — one row per image | Done (ADR 0081) — adjacency-inferred grid, ungroup control remembered for the session | Not started | Grouping is a heuristic: same sender, ≤60s apart, same day, nothing between. A bridge stamping identical timestamps will group images that were never one post |
| Lightbox paging across a timeline | n/a — client-side | **Gap** — single image view | Done (ADR 0081) — arrows, swipe, bounded auto-pagination into history | Not started | Paging spans the timeline rather than a gallery, so a mis-grouped run cannot become a navigation cage; only the counter is run-aware |
| Save a displayed image to the device | n/a — client-side over the existing proxy | Done — download to disk | Done (ADR 0081) — share sheet where the platform takes files, transient anchor otherwise | Not started | `window.open` is banned repo-wide for the eventual Tauri desktop shell (M-W12, groundwork only — not yet released), so the anchor is the only sanctioned path |
| Media thumbnail proxy | Done (M17, ADR 0063) | **Gap** — client-side downscale only; doesn't call the server endpoint | Done | Not started | Called out explicitly in AGENTS.md's M17 note |
| Full-text search | Done (M9) | Done (minimal input, per MVP scope) | Done | Not started | |
| Drafts (cross-device) | Done (M12, ADR 0048) | Done | Done | Not started | |
| Read markers (cross-device, Axon-internal) | Done (M12, ADR 0048) — the underlying `device_state` store was dropped by a force-push and restored in PR 226; the TUI read-marker feature itself was separately reverted (`116b3cb`) and re-landed in PR 217 (`clients/tui/src/app/read_markers.rs`) | Done | Done, plus a `thread_read_markers` namespace TUI doesn't have | Not started | Reverse gap: web is ahead here |
| Inbound ephemeral passthrough (typing, receipts) | Done (M18, ADR 0056) | Done (`app/ephemeral.rs`; typing + read receipts shown as a bottom-border status on the message pane) | Done (`stores/ephemeral.ts`) | Not started | Adam's motivating example |
| Outbound read receipts to homeserver | Done (#278, ADR 0067) — `POST .../rooms/{room_id}/read` | Done — second fire-and-forget spawn in `read_markers::spawn_read_put` | Done (`stores/ephemeral-sender.ts`, fired from `RoomPage`'s read-marker choke point) | Not started | Both piggyback on their client's existing debounced, forward-only read-marker choke point; fire-and-forget |
| Read receipts that cover thread replies | n/a — client-side (ADR 0096); the route already accepts any event id | Not started — `read_targets_for` filters thread members out of the target, so an arrival-newest reply stays unreachable (#207) | Done — `ThreadPanel` names its arrival-max displayed member behind `RoomPage`'s caught-up gate | Not started | Without it, a room whose arrival-newest events are all thread replies badges on every load (#207) |
| Thread read state from other clients' receipts | Done (M18, ADR 0056) — `m.receipt` is forwarded verbatim, `thread_id` included | Not started | Done (`connectThreadReceipts`; a thread read in Element clears here, durably) | Not started | Needs the MSC3771 `thread_id` the passthrough already carries; axon's own receipts stay unthreaded (ADR 0096) |
| Outbound typing notice | Done (M19a, ADR 0068) — `PUT .../rooms/{room_id}/typing` | Done — `app/typing.rs`, driven by `note_draft_activity` + a `flush_due_typing` tick | Done (`stores/ephemeral-sender.ts`, driven by the composer) | Not started | Throttled true, cleared on empty/command/submit/room-switch/idle |
| Interactive SAS device verification | Done (7a-6, ADR 0027/0028) | Done — full emoji-modal flow | **Gap** — `AccountsPage.tsx` only mentions SAS in a placeholder label; no verification flow implemented | Not started | Verified by grep, 2026-07-17 |
| Matrix OAuth QR account acquisition | Done (ADR 0097) — authenticated, polling-based display/scan flows at `/v1/accounts/login/qr`; successful login is accepted only when Axon's new device is cross-signed | Planned — display QR, enter/display check code, poll, cancel | Planned — camera/image scan plus QR display, check-code handling, poll, cancel | Not started | Server-only in this PR; MAS/Synapse interoperability and black-box smoke remain the final integration slice after client UIs |
| Device-list / picker endpoint | Done (M16, ADR 0060) | **Gap** — no picker UI; verification still requires a blind device id | **Gap** — endpoint appears only in generated `schema.d.ts`; no picker component consumes it | Not started | The exact gap M16's own note anticipated |
| Room-state reads (spaces, pins, info, upgrades) | Done (ADR 0084) — typed children/parents, pinned-event, room-info, and upgrade-chain reads | Not started | Done — a browser-local Spaces picker filters direct children; Room Information shows state details, pins, relationships, and upgrade links | Not started | `room_type` on `RoomDto` identifies joined spaces without probing every room |
| Room membership (leave/forget/invite/kick/ban/unban) | Done (M19b, ADR 0068) — `POST .../rooms/{room_id}/{leave,forget,invite,kick,ban,unban}` | Done (ADR 0079 TUI-M19-1) — slash commands `/leave`, `/part`, `/forget`, `/invite`, `/kick`, `/ban`, `/unban` | Partial (M19-W1/W4) — `/leave`, `/part`, `/forget`, `/invite`, `/cancel`, and Room Information invite/cancel-invite; kick/ban/unban remains | Not started | TUI exposes moderation as selected-room slash commands; web M19-W5 will add kick/ban/unban |
| Room entry (join/knock/create) | Done (M19c, ADR 0068) — `POST .../rooms/{join,knock,dm}` and `POST .../rooms` | Not started | Done (M19-W2/W3/W4) — `/join`, `/knock`, Matrix room-link interception, opt-in browser `matrix:` handler, visible find/join plus directory join-from-result, and create-room/DM flows | Not started | Web W4 also wires Room Information member `DM` actions; homeserver-wide user-directory search remains a follow-up server/API gap |
| Room settings (name/topic/avatar/tags) | Done (M19d, ADR 0068) — `PUT .../rooms/{room_id}/{name,topic}`, `PUT/DELETE .../rooms/{room_id}/avatar`, and `PUT/DELETE .../rooms/{room_id}/tags/{tag}` | Not started | Not started | Not started | Server-only; client UI (room settings panel, tag/favorite toggle) is separate follow-up work. `tags` writes `m.tag` room account data, not a state event |
| Power levels | Done (M19e, ADR 0068) — `PUT/GET .../rooms/{room_id}/power_levels` | Not started | Not started | Not started | Server-only; client UI (power-levels editor, self-demotion confirmation) is separate follow-up work. Write merges role thresholds and per-user levels into one `m.room.power_levels` event; rejects a change that would strand the caller below the level needed to self-correct unless `acknowledge_self_demotion` is set |
| Account actions (profile/ignore/directory search) | Done (M19f, ADR 0068) — `PUT .../profile/display_name`, `PUT/DELETE .../profile/avatar`, `GET .../users/{user_id}/profile`, `PUT/DELETE .../users/{user_id}/ignore`, `GET .../directory/public_rooms` | Not started | Partial (M19-W3) — public-room directory search in the rooms index; profile editor, user-profile read, and ignore-list management remain | Not started | `public_rooms` is a paginated read, not a mutation like the other four |
| Invited-room visibility (see incoming invites, accept/reject) | Done (ADR 0091) — persisted `room_invites`, `GET /v1/invites`, `invite.added` / `invite.removed` WS frames. Accept/reject reuse existing join/leave. | **Gap** — no inbox; nothing consumes `GET /v1/invites` or the invite frames | Done — Invites row + `/invites` inbox with Accept/Reject and Accept all/Reject all | Not started | ADR 0091 scoped client UI as "web first", TUI out of scope _for that ADR_ — not a decision against a TUI inbox; known-contact DM auto-join (ADR 0040) is unchanged |
| Presence (inbound + outbound) | Deferred (ADR 0056) | Not started | Not started | Not started | No ADR planned until the lag question is addressed |
| Unread / notification counts | Done (ADR 0070, issue #313) — `RoomDto`'s persisted `notification_count`/`highlight_count` plus the `unread_counts.changed` WS frame | **Gap** — `RoomEntry.unread_count` is a local, live-only heuristic (`app/timeline.rs`); doesn't consume the server-persisted field, so counts still reset on restart | Done (`stores/rooms.ts`'s `unreadCount`/`unreadTotal`, fed by `RoomDto` and `unread_counts.changed`); app-icon badge also ships (ADR 0080) | Not started | Web-client consumption was ADR 0070's own deliberate follow-up and has since landed; TUI is now the outstanding consumer |
| Megolm key backup (originate + honesty) | Done (ADR 0098) — `AccountDto.backup` snapshot, recover auto-enable/join/export-resume, `POST /v1/accounts/{id}/backup/enable`, recover flatten + `redecrypt`/`backup_action` | **Planned** — `/recover` stays compatible without source changes; `/backup enable` and `/status` snapshot are a follow-up | **Planned** — still says "Keys recovered"; badge + enable control are a follow-up | Not started | Server GET cache of `exists_on_server` may lag another client; enable then 409s. No `account.backup` WS in this PR. Recover does not decrypt pre-Axon history |

## Out of scope for this doc

- Server-only infrastructure with no client-visible surface (search index
  internals, backfill, account lifecycle state machine internals) — those
  live in `AGENTS.md`'s "Current state" section.
- Design decisions and rationale — those live in `docs/adr/`.
