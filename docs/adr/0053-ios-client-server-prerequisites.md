# ADR 0053 — Server prerequisites for the iOS client

## Context

The MVP is nearly complete. The team's next priority is a native iOS client
(SwiftUI, per ADR 0031 — not revisited here). Before iOS client work can
begin in earnest, a planning pass surfaced several server-side gaps that
either block shipping or block basic usability. This ADR is an inventory of
that prerequisite work — a punch list, not a design document for any single
item. A separate, later ADR will lay out the iOS client's own milestone
roadmap on top of whatever lands from this list.

Decisions already made that shape this list:

- **OAuth 2.0 + PKCE is a hard blocker before iOS ships.** Unlike `axon-tui`,
  which uses CLI-minted bearer-token paste (ADR 0029), the iOS client is not
  allowed to ship to real users on the paste flow. This is a deliberate
  choice to accept a longer runway in exchange for not shipping a weaker
  auth UX on a new platform.
- **Push notifications (APNs) are explicitly out of scope.** They are not a
  prerequisite for iOS client work to begin. No device-token endpoint, no
  push router, no APNs integration is commissioned here; that remains a
  fully separate, unscoped future concern with its own ADR whenever someone
  picks it up. (ADR 0031's framing of push as a "day-one" client concern is
  accordingly stale — see the ADR 0031 amendment below.)
- **ADR 0031 contains a factual error**, repeated in the PRD, tech-spec, and
  implementation docs: "Generated SDK stubs for Swift already ship as part
  of the MVP build." No Swift package, codegen config, or Swift file of any
  kind exists anywhere in the repository today. This ADR corrects the claim
  in ADR 0031 itself (the `docs/mvp/` documents are frozen at MVP ship per
  `AGENTS.md` and are not edited retroactively).

**Before finalizing this list, open PRs and issues in the repo were checked
for overlap**, which changed the scope materially from the first draft:

- The originally-considered "tag/pin write-through endpoint" item turned out
  to already be substantially in flight and is **not** included below as new
  work. ADR 0048 (accepted, on `main`) already designs exactly this: a
  generic per-device key/value store (`device_state` table, `GET`/`PUT
  /v1/devices/{device_id}/state/{namespace}`, cross-device last-write-wins
  merge, WebSocket fan-out via `device_state.changed`). PR #191 (the generic
  store and endpoint, M12 PR2) and PR #192 (draft sync) are merged. PR #196
  (read markers) and PR #199 (unread-thread attention) were briefly merged
  and then reverted: #196 had no human review, and #199 had an unresolved
  correctness bug flagged in review (the unread-thread picker selects by
  list index into a list that gets re-sorted on live events) that was merged
  over without being addressed. Both are reopened for proper review as
  **#217** (read markers) and **#218** (unread-thread attention, carrying
  forward the unresolved feedback from #199) — i.e., M12 is still in flight,
  not landed, though the team already has an established pattern for
  exactly this class of problem. Once M12 lands, cross-device room-pin/
  favorite sync for iOS is a new namespace on an existing endpoint, not new
  server design. This ADR lists it only as a dependency to track (see
  Consequences), not as a numbered prerequisite.
- A separate, deeper alternative exists and remains genuinely unbuilt: PR
  #174 ("room metadata exposure strategy") proposes real Matrix `m.tag`
  account data sync, visible to non-Axon clients too — not just Axon's own
  devices. (Issue #130's generic API passthrough and its inbound dual, draft
  ADR 0043 (PR #178), are a related but separate effort — ephemeral event
  passthrough for typing/receipts/presence, not account-data tags.) The
  `m.tag` path remains genuinely unbuilt, but iOS does not need to wait for
  it; `device_state` gives parity with what `axon-tui` and the (unmerged,
  draft) web client already do. It is recorded below only as an open
  question.
- The three items below (OAuth, device-listing, ADR 0030 `sync_state`) were
  each checked against every open PR and issue and confirmed **not**
  addressed anywhere in flight. They are genuine, unclaimed gaps.

## Decision

Three items are required prerequisite work, in recommended sequence:

### 1. OAuth 2.0 + PKCE + Sign-in-with-Apple

The long pole. Today the server has bearer tokens only, minted out-of-band
via `axon token issue`, with no `/v1/auth/*` routes of any kind; the only
forward-compatibility seam is the `TokenVerifier` trait carved out by ADR
0029. This ADR does not design the OAuth flow — implementation should be
gated on a dedicated follow-on ADR (authorization-code + PKCE flow, Apple
identity-token verification, session/refresh-token model) before any code is
written, since an authorization server is a real architecture decision in
its own right, not a client-side concern.

Recommendation, open for discussion: scope Sign-in-with-Apple only for v1
(skip Google/Microsoft SSO) to keep the blocking dependency as small as
possible. See Open Questions.

### 2. Device-listing / discovery endpoint

No such endpoint exists anywhere in `openapi/openapi.json` today —
`POST /v1/accounts/{account_id}/verify` takes a bare `user_id` or `device_id`
string with nothing to look it up against. This is needed so iOS (and any
future client) can offer a real device picker for SAS verification instead
of requiring the user to already know the target id blind, which is what
`axon-tui` (ADR 0028) and the draft web client both do today.

### 3. ADR 0030 `sync_state` implementation

ADR 0030 (account sync-state readiness signal) is already accepted but was
never built: `grep -rn sync_state crates/ clients/` returns zero matches.
It needs: the `sync_state` field (`connecting`/`syncing`/`ready`/`offline`)
on `AccountDto`, the `account.sync_state` WebSocket frame, and the 30s
defense-in-depth timeout on mutation routes — all already specified by ADR
0030. No new design is needed here, only implementation. This is the
smallest, lowest-risk item on the list, and unblocks a real "still syncing"
signal for client compose/send UI instead of a guess-and-hope timeout.

### Dependency to track, not new work: cross-device pin/favorite sync

As covered in Context, this rides ADR 0048's `device_state` mechanism once
M12 (PRs #191/#192 merged; #217/#218 open) lands — add a `room-pins` (or
similar) namespace at that point. No new server design is commissioned here.

## Consequences

- Item 1 (OAuth) blocks iOS *shipping*. Items 2–3 block good UX, not a
  functional start, and can proceed in parallel or in either order.
- Cross-device pin sync rides M12's `device_state` once it merges — tracked
  as a dependency, not scoped as new prerequisite work by this ADR.
- Push notifications remain fully out of scope; no device-token endpoint,
  router, or APNs integration is commissioned here.
- OAuth's internal design is deferred to its own follow-on ADR; this ADR
  only establishes that it is required before iOS ships.
- ADR 0031 is amended (see below) to correct the false "Swift stubs already
  ship" claim and to note that server prerequisites for iOS are now tracked
  here, with a separate iOS client MVP roadmap ADR still to come. Its
  Web→iOS sequencing and native-vs-cross-platform discussion are untouched.

## Open Questions

- **SSO provider scope for v1**: Apple-only, or also Google/Microsoft from
  day one? Recommendation: Apple-only, to minimize the blocking dependency.
- **Room-pin sync path**: ride `device_state` (Axon-only visibility, ready
  sooner) or wait for the real Matrix `m.tag` passthrough path (PR #174,
  cross-client visibility, unbuilt)? Recommendation: `device_state` now;
  revisit if cross-client visibility becomes a real requirement later.
- **Owner and timeline for each item**: not assigned. This ADR scopes the
  work; it does not schedule or staff it.

## Amendment — remote push (2026-10-09, ADR 0113)

The push bullets above still describe this list.
They do not commission a device-token endpoint, a router, or APNs.
[ADR 0113](0113-remote-push-for-suspended-mobile-clients.md) is the design this list deferred.
It adds no code.
Implementation is tracked in #604 and #605.
