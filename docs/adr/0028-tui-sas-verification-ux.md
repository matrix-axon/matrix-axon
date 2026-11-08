# ADR 0028 — TUI SAS verification UX

## Context

ADR 0027 settled the **server-side** interactive SAS verification contract
(7a-6, #76): HTTP verbs `POST …/verify {device_id}` / `…/confirm` / `…/cancel`,
read routes `GET …/verify` and `GET …/verify/{flow_id}`, and server→client
`verification.{requested,sas,done,cancelled}` frames on the best-effort `/v1/ws`
bus. That PR was explicitly backend-only; the PRD frames "`axon-tui` can drive
them" as separate client work.

This ADR records the decisions for that client work: how `axon-tui` exposes SAS
emoji verification as a second device-trust path alongside the
recovery-key flow (`/recover`, pending #69). It is written for team review **before**
implementation; the step-by-step build lives in the implementation plan, not
here.

Three decisions shape the UX and are worth recording:

1. **Which verification directions the TUI supports**, given there is no
   device-list endpoint today.
2. **How peer-initiated requests surface**, given SAS flows time out.
3. **How the client tolerates the lossy live bus**, given ADR 0027 makes the
   read API — not the WS stream — the source of truth.

## Decision

### 1. Bi-directional verification; outgoing pastes a device ID until a device-list endpoint exists

The TUI supports both directions:

- **Incoming (peer-initiated).** The user starts verification from a trusted
  client (e.g. Element). axon emits `verification.requested`; the TUI drives the
  flow with **no device ID required**. This is the primary path and matches the
  documented first-run story (see §"Verification (7a)" in
  `docs/mvp/implementation.md`). **Scope note:** the server only accepts
  self-verification requests (`is_self_verification()` check in #76); requests
  from other users are silently rejected server-side and will never surface as
  `verification.requested` frames.
- **Outgoing (TUI-initiated).** `/verify <device_id>` starts a flow against a
  named device, operating on the active account filter (and refusing when the
  filter is "all" and multiple accounts are active, mirroring the targeting logic
  in `app/lifecycle.rs`).

**The device-ID gap is accepted for now.** ADR 0027's contract names the target
device explicitly and there is no endpoint to enumerate an account's devices, so
the outgoing direction requires the user to paste a device ID read off another
client. A device-list endpoint (`GET …/devices`) plus a TUI device picker is
tracked as **issue #84** and is out of scope here. The incoming direction is
fully smooth today and is the one the first-run story depends on.

*(Superseded for axon-tui: bare `/verify` opens a device picker on `GET …/devices` (#333). `/verify <device_id>` remains the paste path, and `/verify @user:server` is unchanged. The list stays hidden until this session's device id is known.)*

### 2. Incoming requests auto-open a modal; emoji comparison is a centered popup

A `verification.requested` frame **auto-opens** the verification modal and takes
focus. SAS flows time out, so a passive notification risks the user missing the
window; auto-opening trades an interruption for a prompt the user can act on.
Once open, the modal obeys the AGENTS.md multi-step-interaction rule — background
WebSocket / refresh / timeline statuses must not overwrite its entry line until
the flow ends.

The SAS comparison is a **centered modal popup** (the `/help` popup pattern), not
an entry-line prompt: it shows the seven emoji with their descriptions, the
decimal triple as a fallback, the current stage, and a `[y]es / [n]o · Esc`
prompt. Seven labeled emoji plus instructions do not fit the entry line. The
modal is its own `Mode` (custom confirm/cancel keys), using literal `y`/`n`/`Esc`
like the existing logout confirmation — no new configurable shortcut.

### 3. The client treats the read API as the source of truth (read-on-reconnect)

Per ADR 0027 the verification frames ride the lossy broadcast bus and a lagging
client silently skips frames. The TUI therefore **never** assumes the frames it
received are complete: while a flow is active, on WS reconnect it re-reads
`GET …/verify/{flow_id}` to resync stage and SAS values, and may `GET …/verify`
to discover a request that arrived while disconnected. This is the read-on-
reconnect contract ADR 0027 requires of clients.

**404 on resync.** A 404 from `GET …/verify/{flow_id}` while a flow is active
is treated as an **implicit server-side cancellation**: the modal transitions to
an error state ("Verification ended — the flow was cancelled by the server") and
waits for the user to dismiss with `Esc`, exactly like a `verification.cancelled`
frame but with distinct messaging. The TUI does not retry, does not attempt to
restart the flow, and does not silently close the modal.

This covers two cases under the same rule: the documented 5-minute TTL expiry
and the `cancel_account_flows` teardown path (sync-run restart, no terminal
frame emitted — flagged in review as a server-side deficiency). Treating them
identically is intentional: from the client's perspective both mean "the server
has no record of this flow"; the distinction is invisible and not actionable by
the user. If the server is later fixed to emit `verification.cancelled` on
sync-run restart, the 404 path degrades gracefully to a fallback — the modal
will already have closed via the frame before the resync fires.

### 4. Verification runs off the event loop, independent of the lifecycle gate

`start` / `confirm` / `cancel` / reconnect-resync are async HTTP calls spawned
off the render loop, with results returning over the existing main-loop channel
(the `LifecycleOutcome` pattern used by login/recover/etc.). Verification is
**not** gated by the `lifecycle_busy` flag — the TUI does not need to serialize
it against login/logout/recover/delete at the client level. However, `start` /
`confirm` / `cancel` share the same per-identity lifecycle lock on the server, so
they are serialized there: a concurrent logout can sever the account mid-flow and
cause a subsequent `confirm` to return 409 / `NotActive`. "Surfaces API error
messages verbatim" covers this operationally; the modal must be prepared to
receive and display such errors. The TUI stays an HTTP + read-only-WS client
(per `clients/tui/AGENTS.md`): it never talks to a homeserver or the Matrix SDK
directly.

## Consequences

- The TUI gains an emoji verification path beside `/recover` (#69). Incoming
  verification is end-to-end usable; outgoing is usable but requires a pasted
  device ID until #84 lands.
- The implementation touches `clients/tui/src/` only: a new client `FlowDto`/
  WS-frame decode and verify methods in `api.rs`, a `Mode::Verification` +
  flow state in `app.rs`, live-frame handling in `app/timeline.rs`, the
  `/verify` command, off-loop dispatch in `app/lifecycle.rs`, the modal render +
  `popup_shortcuts_lines`/`/help` entries in `ui.rs`, and key handling in
  `keymap.rs`. No server, OpenAPI, or shared-crate changes.
- The device-picker UX and any client `accept`/WS-command surface remain follow-
  ups, naturally sequenced with #84 and the 7b auth work (ADR 0027).
- If a device-list endpoint later changes how the outgoing flow selects a target,
  decision (1) is the part to revisit; #84 is the tracking issue.
