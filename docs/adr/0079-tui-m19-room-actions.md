# ADR 0079 — TUI rollout: M19 room and account actions

**Status:** Proposed — TUI-side companion to ADR 0068 and ADR 0069.

## Context

ADR 0068 completed the server-side M19 Matrix Client-Server verb batch, and
ADR 0069 plans the web-client rollout. The web rollout is intentionally split
into nine submilestones because web needs separate panels, click targets,
protocol-handler UI, and richer member/settings surfaces.

`axon-tui` can cover the same M19 server contracts with fewer submilestones:
its user-facing surface is slash-command driven, short responses already fit in
the entry box or command-response popup, and multi-step flows already use
explicit modes plus background outcome channels. M19a typing notices are already
implemented in the TUI and are not part of this rollout.

Tier-C work from issue 279 remains out of scope: invited-room visibility still
needs stripped-state server projection, presence remains deferred per ADR 0056,
and unread/notification counts are tracked separately.

## Decision

Implement the TUI rollout as **five TUI-only PRs**, each using the implemented
M19 wire contracts on `main` and updating `docs/client-parity.md` in the same
PR. No server or OpenAPI changes are part of this ADR.

Shared TUI rules:

- Add typed `AxonClient` methods and DTOs for M19b-M19f. Empty-object
  mutations ignore the response body; reads deserialize typed payloads.
- Run every new network action off the key-handling path. Results return through
  a room-action outcome channel or an existing read-flow channel, then mutate UI
  state on the main loop.
- Use authoritative refreshes after successful writes. Membership, settings,
  tags, and power levels must not treat optimistic local state as the source of
  truth.
- Keep user-visible surfaces command-first: entry-line status for short
  outcomes, existing scrollable popups for long read/list/editor-like output,
  and explicit confirmation modes for destructive or moderation writes.
- Keep command help, tab completion, README command docs, and client parity in
  sync with each PR.

## TUI Submilestones

1. **TUI-M19-1 — Membership and moderation.** Implement `/leave`, `/part`,
   `/forget [room]`, `/invite <user>`, `/kick <user> [reason]`,
   `/ban <user> [reason]`, and `/unban <user> [reason]`. `/forget <room>`
   accepts the same room target forms as `/room`; other verbs target the
   selected room. Leave, forget, kick, ban, and unban require confirmation.
   Invite sends immediately. Successful writes refresh rooms and member caches.
2. **TUI-M19-2 — Entry, directory, and creation.** Implement `/join`,
   `/knock`, `/directory`, `/dm`, and `/create-room`. Parse `matrix.to` and
   `matrix:` room links with `ruma`; show directory results in a selectable,
   paginated popup where Enter joins the selected room. Do not automatically
   retry create-room/create-DM after timeouts because those server operations
   are not idempotent.
3. **TUI-M19-3 — Room settings and tags.** Implement `/roomname`,
   `/topic`, `/roomavatar`, `/tag`, `/untag`, `/favorite`, and `/unfavorite`.
   Clears use explicit `--clear`, not blank input. Avatar writes reuse staged
   image upload and require an image content type. Tag-backed room-list
   display/filtering waits on ADR 0103's read surface (issue #365 / #369);
   `/pin` becomes that consumer rather than a parallel `/favorite` in the
   first landing.
4. **TUI-M19-4 — Power levels.** Implement a read-only `/powerlevels` popup and
   conservative `/powerlevel set ...` edits. Always fetch current resolved
   levels first, write one merged request, and require explicit confirmation
   before sending `acknowledge_self_demotion=true`.
5. **TUI-M19-5 — Account and user actions.** Implement `/profile`,
   `/displayname`, `/avatar`, `/userinfo`, `/ignore`, and `/unignore`. Own
   avatar writes reuse staged upload; user-profile reads show display name and
   avatar MXC in the existing popup/status response pattern.

## Consequences

- **Pro:** The TUI reaches M19 parity with roughly half the web milestone count
  while preserving small review surfaces.
- **Pro:** The plan fits existing TUI mechanics: slash commands, explicit modes,
  background outcomes, and authoritative refreshes.
- **Con / accepted:** Some richer UI affordances, such as tag-backed room-list
  display and full settings panels, wait for read surfaces or a later polish
  pass rather than being invented locally.
- **Con / accepted:** The TUI does not register an OS-level `matrix:` protocol
  handler; users paste Matrix links into `/join` or `/knock`.

## Verification

Each TUI PR runs:

```bash
cargo fmt --all --check
cargo test -p axon-tui
cargo clippy -p axon-tui --all-targets --all-features -- -D warnings
```

Broaden verification only when a later PR intentionally changes shared crates,
server contracts, or workspace dependencies.
