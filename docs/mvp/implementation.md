# Axon MVP — Implementation Specification

**Audience:** an agentic coder (Claude Code or similar) scaffolding and building the Axon MVP. Reads top to bottom; a coder following it without reading the other docs should be able to scaffold the workspace, run Postgres, and reach Milestone 1.

Related docs: [`prd.md`](./prd.md), [`tech-spec.md`](./tech-spec.md).

## Project layout

End-state target. Create incrementally as milestones land.

```
matrix-axon/
  Cargo.toml                 # workspace
  AGENTS.md                  # canonical orientation for agentic contributors
  CLAUDE.md                  # one-line pointer to AGENTS.md
  crates/
    axon-server/             # binary; wires components together
    axon-core/               # shared types, errors, config
    axon-store/              # Postgres + sqlx; event store, account data
    axon-sync/               # matrix-rust-sdk sync engine wrapper
    axon-crypto/             # RESERVED STUB — verification lives in axon-sync (ADR 0027)
    axon-search/             # Tantivy index
    axon-media/              # media proxy + disk-cache backend
    axon-api/                # axum HTTP + WS handlers, OpenAPI (utoipa)
  (each crate has its own README.md + crate-level //! rustdoc)
  clients/
    tui/                     # axon-tui (terminal client; the alpha client)
  openapi/                   # spec source of truth (handwritten + utoipa-emitted)
  docs/
    mvp/                     # this directory
    adr/                     # architecture decision records
    self-hosting.md          # produced in Milestone 13
  docker-compose.yml         # Postgres for dev
```

`axon-tui` is the alpha client — a terminal client that exercises the full `/v1/` surface end-to-end. It replaced the originally-planned `axon-web` (Vite + React) as the reference client; the API is the deliverable, and the TUI is the integration surface that proves it. A web client remains a credible later addition consuming the same API.

## Settled stack

- **Language:** Rust. Pick a recent stable edition; pin MSRV in `Cargo.toml` once initial scaffolding lands.
- **HTTP / WS:** axum.
- **DB:** Postgres via sqlx (compile-time-checked queries).
- **Matrix:** matrix-rust-sdk (sync, olm/megolm, key backup, cross-signing, verification surface).
- **Search:** Tantivy.
- **OpenAPI:** utoipa for type-checked spec emission from handler signatures.
- **Alpha client:** `axon-tui`, a Rust terminal client, replacing the originally-planned Vite + React web alpha.
- **Client stubs:** an OpenAPI-to-Swift generator for the deferred iOS client (run but unused at MVP); TypeScript stubs remain available for a future web client. `axon-tui`, being Rust, consumes the API types directly.
- **Media backend:** local disk LRU cache. No S3 adapter in MVP.

## Settled decisions inherited from [`tech-spec.md`](./tech-spec.md)

Read the tech spec before starting. Highlights that gate implementation:

- One Axon per human, N Matrix accounts inside. Every account-scoped table carries `account_id`.
- Event provenance: `events.provenance` defaults to `upstream_homeserver`.
- Event schema is hybrid hot-columns + JSONB. `origin_ts` is `bigint` milliseconds since Unix epoch (matches Matrix `origin_server_ts`).
- Redactions are stored as events with `type = m.room.redaction` and `redacts = <event_id>`; the target row's content is masked at read time, original ciphertext / megolm metadata preserved.
- Live updates: WebSocket at `/v1/ws`, envelope `{type, account_id, payload}`.
- Auth: bearer tokens minted by an `axon` CLI subcommand.
- API versioning: all routes under `/v1/`.
- Sync: Simplified Sliding Sync only.
- Search: single language-agnostic Tantivy analyzer; `account_id` is a facet field.
- Bridges: pass through, no normalization.
- Onboarding: fresh sync only.
- Push: not in scope; do not write push code paths.

## Milestones

Each milestone has explicit deliverables and a verification step that exercises real behavior, not just `cargo check`. Stop and ask before deviating; if an ambiguity arises that the specs do not cover, raise it instead of picking silently.

> **Status:** Milestones 1–14 have shipped (through the OAuth authorization server, M14), along with M15 (media send/upload), M16 (device-list endpoint), M17 (media thumbnail proxy), M18 (ephemeral passthrough), and M19 (Matrix C-S verb batching, all six PRs). **MVP has not shipped.** The gate is M13 (deployment docs): `docs/self-hosting.md` and the deployment recipes below are still unwritten, and client-side coverage of the M19 server verbs (room settings, power levels, account actions) hasn't landed on either `axon-tui` or `axon-web` yet — see `docs/client-parity.md`. This document tracks the _plan_, not live progress — for what's actually built and in flight, see `AGENTS.md` "Current state" (kept here to avoid two sources of truth drifting) and `docs/client-parity.md` for the per-client picture. The milestone sequence has been resequenced and extended more than once since this document was first written (see the table near the end, now extended through M19); expect it to keep evolving until MVP ships.

### 1. Workspace scaffolding

- Create the Cargo workspace per the project layout.
- Empty crates with `lib.rs` / `main.rs` stubs and minimal `Cargo.toml` files.
- `docker-compose.yml` running Postgres 16 with a named volume.
- Basic CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- Create the initial `AGENTS.md` and a one-line `CLAUDE.md` pointer (see "Documentation for agentic contributors" below).
- Seed `docs/adr/` with `0001-record-architecture-decisions.md` (the meta-ADR adopting the practice).

**Verification:** `docker compose up -d postgres && cargo build` succeeds; CI passes on the first push.

### 2. Config + bootstrap

- Config loader in `axon-core` (figment or config-rs). Sources: TOML file + env overrides.
- `axon-store` opens a sqlx pool, runs migrations from `crates/axon-store/migrations/`.
- `axon-server` starts an axum server with a `/healthz` route.

**Verification:** Run `axon` against the docker-compose Postgres; `curl localhost:PORT/healthz` returns 200.

### 3. Sync engine v0

- `accounts` table in `axon-store`: `(account_id, user_id, homeserver_url, device_id, access_token_encrypted, sync_token, created_at, …)`.
- `axon-sync` wires one matrix-rust-sdk `Client` per account. MVP provisions a single account from config but the code path iterates over all rows in `accounts`.
- Run Simplified Sliding Sync; persist raw + decrypted events into `axon-store` scoped by `account_id` with `provenance = 'upstream_homeserver'`.

**Verification:** Point `axon` at a Synapse running in docker; log in a test account; watch decrypted rows accumulate in the events table over a fresh sync.

### 4. Event store schema

- Hybrid hot-columns + JSONB.
- Hot columns: `event_id`, `room_id`, `account_id`, `sender`, `origin_ts` (bigint ms), `type`, `redacts`, `relates_to`, `decrypted_body_text`.
- Full decrypted content as JSONB.
- Sibling tables for original ciphertext and megolm session metadata, linked by `event_id`.
- Indexes: `(room_id, origin_ts DESC)`, `(account_id, room_id)`, unique `(event_id)`, partial index on `redacts` where not null.
- Timeline read by room with pagination, reverse-chronological by default (cursor on `origin_ts`).
- Redaction handling: timeline reads mask `decrypted_body_text` for redacted events and emit a `redacted_because` field; original ciphertext / megolm metadata stay in sibling tables.
- Account data and room state tables.

**Verification:** SQL queries: paginate the most recent N events in a room reverse-chronologically; redact an event and confirm timeline reads mask its content while ciphertext sibling row remains; cursor-based pagination returns stable results across calls.

**E2EE key acquisition & device trust (ADR 0011).** A fresh `axon` device is unverified, so encrypted rooms show UTDs until it obtains keys. Two complementary paths; this milestone builds both pieces that can stand alone, while the interactive verification UX that exercises the second lands in **M7a** (over the M5 WebSocket):

- **Recovery-key bootstrap (build here, end-to-end).** `client.encryption().recovery().recover(key)` restores both the Megolm key backup (history) and the cross-signing private keys (so `axon` self-verifies and future keys flow). Add `sync.account.recovery_key`, encrypted at rest like the access token (ADR 0008) — prefer transient-only handling of this crown-jewel secret. This path needs no client, so it's what lets this milestone **prove decryption end-to-end before any front-end exists**.
- **Verification plumbing (build here, exercised in M7a).** The programmatic SDK flow — surface a `VerificationRequest`, `accept()`, read `sas.emoji()` / `sas.decimals()`, `confirm()` / `cancel()`. "Headless" means `axon` has no UI of its own, not that it can't verify; the SDK API is fully programmatic. The user-facing emoji exchange can't be exercised until the `/v1/ws` WebSocket exists (M5); the interactive verification UX itself was deferred and now lands in **M7a** alongside the rest of the Matrix-account lifecycle. Note: the layout originally called `axon-crypto` "the thin verification surface over rust-sdk crypto," but the M7a-6 implementation lives in `axon-sync` (it needs `ClientManager` / identity locks / supervision); `axon-crypto` stays a reserved stub — see ADR 0027 for the boundary rationale.

**Verification (E2EE):** against a real homeserver with key backup enabled, supply `sync.account.recovery_key`, confirm UTD rows flip to `decrypted = true` as backed-up keys arrive, and that `axon` shows as a verified/cross-signed device.

### 5. Client API v0

- axum routes under `/v1/`:
  - `GET /v1/rooms` (list rooms across all accounts; filterable by `account_id`; sorted by most-recent activity).
  - `GET /v1/rooms/{room_id}/timeline` (paginated, reverse-chronological by default).
  - `GET /v1/events/{event_id}`.
- WebSocket at `/v1/ws`. Envelope: `{type, account_id, payload}`. Live timeline events fan out from the sync engine.
- OpenAPI spec emitted via utoipa; written in parallel.
- TypeScript stubs (deferred — they targeted the dropped web alpha; `axon-tui` consumes the Rust API types directly, and a future web client can regenerate them from the OpenAPI spec).
- Define a shared `ApiResponse<T>` / `ApiError` envelope type in `axon-api` and a
  custom `IntoResponse` impl so all handlers return consistent JSON shapes and error
  bodies. (Deferred from M2; designing against zero real handlers is premature.)

**Verification:** Boot the server, `curl /v1/rooms`, hit `/v1/rooms/{id}/timeline`, open a websocat session to `/v1/ws` and see live events arrive tagged with `account_id` as new events come in over sync.

**Interactive verification UX deferred to M7a.** The SAS-emoji exchange over `/v1/ws` — formerly planned here as "M5c", consuming the M4 plumbing — was never built. It now lands in **M7a**, where it belongs conceptually: verifying `axon`'s _device_ for a _Matrix account_ is part of that account's lifecycle. M5 ships the `/v1/ws` channel the exchange rides on; M7a wires the flow. See M7a.

### 6. Mutations

- `POST /v1/rooms/{room_id}/send` (send message; payload includes `account_id`).
  `body` is plain text; optional `format` + `formatted_body` (paired,
  `org.matrix.custom.html` only) carry rich text verbatim (issue #77).
- `PUT /v1/rooms/{room_id}/events/{event_id}` (edit; same optional formatting).
- `DELETE /v1/rooms/{room_id}/events/{event_id}` (redact).
- `POST /v1/rooms/{room_id}/events/{event_id}/reactions` (react).
- All routed through matrix-rust-sdk's send path on the appropriate `Client`.

**Verification:** Send a message via curl, watch it round-trip through sliding sync, appear in the timeline, and arrive over WS. Redact and confirm the timeline read masks content.

### 7. Account lifecycle and auth

Auth and trust, split into three subphases. **7a** brings the _Matrix_ accounts under runtime control (login, verify, recover, logout, delete) and finally closes the interactive-verification work deferred from M5 (the old "M5c"). **7b** puts the _client ↔ axon_ bearer-token gate in front of the whole API. **7c** delivers _sender-device trust_: evaluating and exposing whether the devices that **other** people sent from are cross-signed, so clients can flag messages from unverified senders. Said another way: 7a is auth between `axon` and the homeserver(s), 7b is auth between a client and `axon`, and 7c is the trust `axon` reports about the _senders_ of the events it stores.

#### 7a. Homeserver account lifecycle & verification

Today an account is provisioned exactly once from config, and there is no supported way to add, verify, or remove one at runtime. Changing `sync.account.user_id` in config does **not** replace the account — it inserts a new `accounts` row and strands the old one, which keeps syncing and can still _send_ (any row with a decryptable token gets connected). This was hit in a real debugging session: a message went out authored by a previously-configured account that was no longer in config. 7a makes the Matrix-account lifecycle a first-class API and folds in the interactive device verification deferred from M5. It is the milestone that closes GH issues #14 (stale-DB cleanup) and #24 (account lifecycle / active-account gating / runtime provisioning).

**Config becomes optional past 7a.** The minimal boot configuration should be just the things that need server-side plumbing before anything else can run — Postgres connection + the `store_key`. Everything account-shaped is then configurable through the API: mint the first token via the `axon` CLI (7b, DB-only), then `POST /v1/accounts/login`. The existing `sync.account.*` config drops from _required_ to an optional bootstrap convenience (and is a candidate for removal once the API path is the norm). The design target: `axon` boots clean with no accounts and waits for the API to provision them.

**Account state machine.** Add an explicit lifecycle `state` to `accounts` — `active` or `deactivated`, plus a transient `deleting` (below) — kept _orthogonal_ to verification status (a device can be `active` but not yet verified: it syncs and shows UTDs until it acquires keys). The sync engine **and** the M6 mutations gateway connect and serve **only `active` accounts** — never "any row with a decryptable token," the bug behind #24; `get_or_connect` gates on `state`. `deactivated` is a **reversible pause that retains all data** — a stale or token-expired account stops syncing and sending but is not erased (a natural home for the per-account failure isolation in ADR 0010). This is _not_ a soft-delete model: `deactivated` is the soft stop (reached via `logout` or internal token-failure); deletion (via `DELETE`, below) is a hard removal of the row, not a resting `deleted` tombstone. The third state, **`deleting`**, is _transient_ — a crash-recovery breadcrumb set while teardown is in flight and gone once the row is, never a resting state a client observes long-term. There are no dedicated state-setter endpoints — `state` is a consequence of the lifecycle verbs (`login` → `active`, `logout` → `deactivated`, re-`login` → `active`) plus internal failure handling, never a value a client PUTs directly.

**Lifecycle endpoints** (account-nested per the M5a convention). These are destructive (`DELETE`) and secret-bearing (`login`/`recover` accept a password and a recovery key), so they must not sit open on the network before auth exists. Until the 7b token gate lands they are **bound to loopback (`127.0.0.1`) only** — local administration on the host, never the public bind address. Consequently the M7a-without-7b intermediate state is **not remotely deployable**: M13's private-mesh-VPN guidance is defense-in-depth for the _whole_ `/v1/` surface once 7b ships, not a substitute for application auth on these specific endpoints (M13 can't retroactively gate an earlier milestone). Once 7b lands, the loopback restriction lifts and the bearer gate applies to them like every other route.

- `GET /v1/accounts` and `GET /v1/accounts/{account_id}` — list / read accounts with their lifecycle `state`, verification status (is `axon`'s device cross-signed yet), and sync progress. This is what a client polls to decide whether to prompt the user to verify-or-recover, and it's the read side of the lifecycle (never exposes the token or other secrets).
- `POST /v1/accounts/login` — body `{ homeserver_url, username, password }`. **Identity is keyed by canonical `(user_id, homeserver_url)`** (the `accounts` upsert key), with one unambiguous rule: a login for a pair not currently present **mints a new `account_id`**; a login matching an existing **`deactivated`** account **reactivates that same row** — reusing its `account_id` and its **retained archive** (the decrypted events + search index), _not_ minting a new `axon`-side identity; a login after a hard `DELETE` (the row is gone) mints a fresh `account_id`. In every case `axon` logs in as a **fresh Matrix device** — logout invalidated the prior device's token upstream, so that device is dead and its SDK store can't be reopened under a newly-issued device ID — provisions a **fresh per-account SDK store** for the new device, encrypts the new access token at rest (ADR 0008), **reacquires room keys via `recover`/`verify` (below) before switching the device into service**, and starts sync. Reactivation therefore restores the **archive**, not the dead device's crypto/session store: the `account_id` is `axon`'s stable handle, decoupled from the Matrix device identity behind it. This is the supported way to add accounts 2…N without swapping config and stranding the prior account. The `password` is consumed once and never stored (matches the M3 login path) — a crown-jewel secret handled transient-only. Concurrent `login` / `logout` / `delete` on one account are **serialized by a single async lifecycle lock** (so a stop never races a re-spawn) and each verb is idempotent.
- `POST /v1/accounts/{account_id}/verify` — **starts** an interactive SAS (emoji) exchange; a single request can't describe the whole state machine, so the protocol is explicitly asynchronous:
  - **Transaction.** The request body **names the target device** (`{ device_id }`) — one of the user's other trusted devices — rather than leaving target selection implicit; `verify` returns a `flow_id` (the verification transaction). The state machine is keyed by `(account_id, flow_id)`, so concurrent exchanges don't collide.
  - **Operations.** `confirm` / `cancel` as HTTP (`…/verify/{flow_id}/confirm`, `…/verify/{flow_id}/cancel`). **The shipped 7a-6 contract is HTTP-only and has no explicit `accept` verb** — the protocol-level accept needs no human decision, so the driver performs it automatically; only `confirm` (the emoji comparison) is a client decision. The "equivalent `/v1/ws` commands" and a client `accept` operation are **deferred to the 7b auth work** (a reliable bidirectional command channel needs the client identity 7b establishes); the WS socket stays server→client send-only. See ADR 0027 for the rationale. **Flow state is readable out-of-band:** `GET …/verify` lists the account's active flows and `GET …/verify/{flow_id}` returns one flow's replayable state — current stage, target device, and the SAS `emoji()` / `decimals()` (re-derivable from the live `SasVerification` object while the flow is alive). This is the endpoint a reconnecting client reads to resume.
  - **Frames.** Server→client `verification.{requested,sas,done,cancelled}` over `/v1/ws` carry the `sas.emoji()` / `sas.decimals()` and the outcome. A **peer-initiated** request surfaces as `verification.requested` with no HTTP kickoff.
  - **Timeouts / cancellation / reconnect.** A flow times out and auto-cancels; either side can `cancel`; on emoji mismatch the user cancels. A client that drops and reconnects does **not** assume the old socket's frames — it re-reads per-flow state via `GET …/verify/{flow_id}` (account-level status can't carry the live `flow_id`, stage, target device, or the SAS values that may have arrived on the lost socket; those values are re-readable from the live flow, so nothing one-shot is lost). Operations are idempotent. SAS is notoriously under-implemented across Matrix clients — getting reconnect, cancel, mismatch, and timeout right is an explicit goal here, not a nice-to-have.

  After mutual confirm, `axon` is cross-signed and the user's other devices **gossip** the cross-signing secrets and the key-backup key — so the recovery key never has to live server-side. This is the mature key-acquisition path (ADR 0011).

- `POST /v1/accounts/{account_id}/recover` — the bootstrap path: accept a Secure-Storage (4S) recovery key and call `client.encryption().recovery().recover(key)`, which imports the megolm key **backup** and the cross-signing private keys into the per-account crypto store. Two effects: (1) holding the recovered user-signing key lets `axon` **self-verify its own device** with no interactive partner — "verify a device via backup-key recovery"; and (2) the imported keys let the existing M3c re-decryption queue flip already-stored UTD rows to `decrypted`. It does **not** fetch _history_ — recovering _keys_ is not the same as fetching _messages_ (ADR 0011/0018). Pulling a room's pre-install timeline is **M10 backfill**, which consumes exactly these keys; this is why M7a (acquire keys + verify the device) precedes M10 (use them). The recovery-key _string_ is transient-only — never persisted, consistent with the M3c boot-time `recover()` — but note that is distinct from the imported _backup keys_, which persist durably in the crypto store, so M10 still has them.
- `POST /v1/accounts/{account_id}/logout` — invalidate the device's access token upstream and move the account to `deactivated`, **retaining all of `axon`'s data** (the decrypted archive, search index, media cache stay). A logged-out device's token is dead, so the account can't sync or send anyway — but the archive is the whole reason `axon` exists, so logout keeps it. Reversible: a fresh `login` re-authenticates and returns the account to `active`. This is the _non-destructive_ stop.
- `DELETE /v1/accounts/{account_id}` — the destructive teardown. "Every trace" spans more than Postgres, so this is an **ordered, idempotent, crash-recoverable** operation, and the order is load-bearing: the row is the only durable key that lets a reconcile re-find the _external_ resources, so **the row is deleted last**. (1) move the row to **`deleting`** — a durable marker that survives a crash, recording that external cleanup is still owed — and invalidate the token upstream if still live; (2) **cancel the account's in-flight sync/backfill tasks**; (3) **evict its cached SDK `Client`** (so no live handle holds the store dir); (4) **delete its documents from the Tantivy search index** (a DB cascade can't reach the index — M9); (5) remove the on-disk SDK store at `data_dir/<account_id>/`; (6) **purge its entries from the media cache** (M11); (7) **only now delete the `accounts` row** — FK cascades drop `events` / `account_data` / `room_state`. Removing the row last is the crash-safety guarantee: if the process dies mid-teardown, a row left in `deleting` at boot tells the reconcile to re-run the sequence from the top. It cannot lean on orphan-store-dir detection for this — that catches the SDK dir but never the search docs or media entries, which are keyed by `account_id` and would be stranded forever if the row vanished first. No resting tombstone is kept; once teardown finishes the row is gone, and re-adding the same Matrix account later is a fresh `login` with a new `account_id`. This replaces today's manual DB surgery (#14). (Client and docs should make the logout/delete distinction explicit — "log out, keep history" vs "delete account, remove everything.")

**Store-dir GC.** Deletion removes the per-account store dir; a boot-time reconcile prunes _orphan_ store dirs under `data_dir/` — those with **no matching `accounts` row at all (any lifecycle state)**, _not_ "no active account." Keying GC off row existence rather than lifecycle state is the load-bearing distinction: a `deactivated` account is a real row that may be reactivated, so GC must never prune its dir on the basis of a transient paused state — pruning by "not active" is exactly the failure mode behind #24 (5 genuine row-less orphans were observed there). Note the dir itself is _not_ what makes reactivation work: reactivation logs in a **fresh device with a fresh store and reacquires keys** (see `login`), so a deactivated account's old dir holds a now-dead device's crypto state and is simply cleaned up when the account is finally `DELETE`d. Orphan-dir GC is thus a backstop for row-less dirs only; the `deleting`-state recovery above is what drives the rest of a teardown to completion. (The location and configurability of `data_dir` itself — XDG / macOS conventions rather than sitting next to the binary — is tracked separately in #45.)

**Verification status.** Persist per account whether `axon`'s device is verified / cross-signed (distinct from lifecycle `state`), so the API can report key-acquisition state and a client can prompt verify-or-recover while the device is still unverified. This is **not** a write-once boolean — a stale "verified" is worse than none: it is re-derived from the SDK's _current_ cross-signing / device state and invalidated when that changes (the device's trust is reset, cross-signing is rotated, etc.), so what the API reports tracks reality rather than a one-time flip.

Out of scope here but explicitly tracked: `store_key` rotation (one key decrypts every account's token) stays deferred (ADR 0008), noted against #24 so it isn't lost once multi-account raises the stakes. Per-account _authorization_ scoping remains a non-goal — one human owns all their accounts.

**Verification (7a):** `POST /v1/accounts/login` against a real homeserver provisions a second account that syncs independently; from a trusted Element session drive `POST …/verify`, watch the SAS emoji arrive over `/v1/ws`, confirm both sides, and see `axon` become cross-signed and subsequently-sent encrypted messages decrypt without a recovery key. Alternatively `POST …/recover` with a 4S key flips already-stored UTD rows to `decrypted` and marks the device verified (without fetching history — that's M10). `POST …/logout` moves the account to `deactivated` — confirm it neither syncs nor sends while its archive is retained. Crucially, confirm **logout → restart `axon` → fresh `login` reactivates the same `account_id` with its archive intact** — as a _fresh_ Matrix device that reacquires keys via `recover`/`verify`, _not_ a resurrection of the dead device's crypto store (the reconcile must not have pruned the deactivated row's dir, and reactivation must also succeed without depending on its contents). `DELETE /v1/accounts/{id}` removes the row, its search-index docs, and the SDK store dir entirely — confirm the deleted account's messages no longer appear in search, and that a crash injected mid-teardown leaves a `deleting` row that the next boot's reconcile drives to completion. Exercise the SAS flow's unhappy paths, not just the happy one: cancel, emoji mismatch, timeout, and a mid-flow client reconnect (state recovered via `GET …/verify/{flow_id}`, not account status). Once 7b lands, confirm a remote or unauthenticated lifecycle call is rejected.

#### 7b. Client ↔ axon bearer-token auth

The local-API gate. (This is the work formerly numbered M8.)

- `axon token issue --label <name>` CLI subcommand: mints a random token, stores a hash, prints the token once. `axon token list` and `axon token revoke <id>`.
- `tokens` table: `(id, label, hash, created_at, last_used_at, revoked_at)`.
- axum middleware validates `Authorization: Bearer …` on every `/v1/…` route — including the 7a lifecycle endpoints — updates `last_used_at`, and rejects revoked tokens.
- WebSocket auth: token in `Sec-WebSocket-Protocol` or the initial envelope, validated on accept.

Design the token storage and middleware so a future OAuth 2.0 + PKCE issuer can replace the CLI mint path without breaking the on-the-wire `Authorization` header or any consumer code. The first token is minted by the CLI (bootstrap); clients carry it thereafter. Until 7b lands, the read/mutation routes are unauthenticated like the rest of the pre-auth API, and the destructive/secret-bearing 7a lifecycle endpoints are restricted to loopback (above) — so there is no remotely-reachable unauthenticated lifecycle surface at any point. M13's VPN is then defense-in-depth over the whole authenticated API, not a stand-in for app auth.

**Verification (7b):** Issue a token; hit `/v1/rooms` with and without the header; revoke; confirm the next call is rejected. Confirm the M6 txn-id retry-duplication caveat is now attributable to a token.

#### 7c. Sender-device trust & content authentication

7a verifies `axon`'s **own** device. This subphase is about a _different_ set of devices: the ones that **sent** the messages `axon` ingested — i.e. _other Matrix users'_ devices in your encrypted rooms. It is the standard Matrix per-message "shield": when you read a message from Bob, was it sent from a device Bob himself has cross-signed (so it's really Bob), or from some new/unverified device on his account (possible impersonation or a compromised account)? `axon`, being a real Matrix device, holds the cryptographic evidence to evaluate that; 7c surfaces it per event so a client can badge "⚠️ sent from an unverified device."

**This is not about the clients that connect to `axon`.** Those (the TUI, a future web app) are _not_ Matrix devices and are never Matrix-verified — they authenticate to `axon` with a bearer token (7b), full stop. And nothing here is "verified with the homeserver": in Matrix the homeserver is untrusted; device trust is **cross-signing between users' own keys**, peer-to-peer. So three independent things: _(1)_ `axon`'s own device trust (7a), _(2)_ client→`axon` token auth (7b), _(3)_ the trust `axon` reports about the **senders** of stored events (7c, here).

E2EE is a core rationale for Matrix, and authenticating _who actually sent_ each message is half of it — without this signal, someone who pops Bob's account could inject a message from a fresh device and the reader sees nothing. ADR 0011 deferred originating verification of other people's devices; 7c picks up the _evaluation-and-display_ half of that deferral. (Actively running SAS against another _user_ stays out of scope; 7c reports trust, it doesn't establish it interactively.)

The storage _partly_ exists but is not yet usable behavior. ADR 0015's `event_sender_device_keys` sibling persists the sending device's **identity keys and a verification-state verdict at decryption time** — a snapshot. It does **not** persist the sender's full cross-signing chain, so 7c does not pretend to read one from disk: the durable record is the at-decryption _verdict_ (below), while the richer chain detail is fetched **live** from the SDK when a client asks for the bundle. The tech-spec's "Content authentication" section already promises an opt-in verification-bundle API; nothing currently _evaluates_ or _exposes_ any of this — 7c closes the gap and makes that promise real.

- **Evaluate sender trust at decryption.** When an event decrypts, record whether the sending device was cross-signed by the sender's own master key (SDK `Encryption`/`Identity` surface) — a `sender_trust` of `verified` / `unverified` / `unknown` (no device keys) / `verification_violation` (the sender's identity was already in conflict when the event arrived; a violation that emerges _later_ is the overlay in the last bullet, not a change to this value). This is a **snapshot at decryption time** — what Matrix's evidence said when the event arrived — and is deliberately _distinct_ from the sender's **current** device trust, which can change afterward. The two can differ (a device trusted when it sent can later be revoked, and vice versa), so they're reported as separate facts: the per-event snapshot below, plus current trust available via the verification bundle. Persist the snapshot alongside the existing sender-device-keys sibling.
- **Expose it on reads.** Add a `sender_trust` field to the timeline `EventDto` (and the live `/v1/ws` frames), so a client can badge a message — exactly what lets `axon-tui` put a warning glyph on messages from unverified devices, like it already does for misleading URLs.
- **Verification bundle (delivers the tech-spec promise).** `GET /v1/accounts/{account_id}/events/{event_id}/verification` composes the **durable at-decryption snapshot** (sending device identity + verdict) with a **live SDK lookup** of the sender's current cross-signing chain and device trust, plus megolm session provenance — for clients that want to show or audit it. If the sending device is no longer available upstream (deleted, or the identity is gone), the bundle returns the stored snapshot with the current-trust portion marked `unknown` rather than failing. Opt-in; ordinary reads carry no extra overhead.
- **Trust-state changes after receipt (MVP-lean).** The at-decryption snapshot is **immutable** — it's the historical fact of what the evidence said when the event arrived, and is never rewritten. Current trust is a **separate overlay**: full re-evaluation when a sender later becomes verified/unverified is a fast-follow, and the MVP minimum is to surface a `verification_violation` _as an overlay_ on affected events when the SDK reports a sender identity change — so a client isn't silently showing a now-distrusted message as trusted, _without_ mutating the original snapshot underneath it.

**Verification (7c):** in an E2EE room, a message from a cross-signed device reads `sender_trust: verified` and one from an unverified device reads `unverified`; `axon-tui` badges the latter; the verification-bundle endpoint returns the device identity + (live) cross-signing evidence for a given event; a sender identity change surfaces a `verification_violation` _overlay_ on the affected events while their at-decryption snapshot stays unchanged.

### 8. Relation aggregation

Matrix expresses edits, reactions, and threads as _relation_ events whose `m.relates_to` carries a `rel_type` — `m.replace` (edit) / `m.annotation` (reaction) / `m.thread`. **Replies are shaped differently** and are a common trip-up: a reply has _no_ `rel_type`; it nests `m.relates_to.m.in_reply_to.event_id` pointing at the replied-to event. `axon` already stores the whole `m.relates_to` block in the `relates_to` hot column (ADR 0015), so both shapes are on disk. But reading them raw forces every client to re-aggregate over whatever timeline window it happens to hold, which silently breaks for relations that land _outside_ that window — a reaction or an edit that arrives long after the original message. The TUI hit exactly this: reactions and edits to messages older than the loaded 50-event slice are dropped (GH issue #22). M8 moves aggregation server-side so the API serves resolved, complete views regardless of pagination.

This **subsumes the formerly-standalone Threads milestone** (old M13): a thread is just the `m.thread` case of the same machinery, and the store already captures `m.thread` generically, so the work is additive and backfill-free — it resolves over whatever rows are stored now, and applies automatically to the deep history M10 backfills in later. Split: 8a builds the store-layer aggregation, 8b exposes it over the API.

#### 8a. Aggregation backend

- Indexes over `events.relates_to` must cover **both** relation shapes, or replies are silently unfindable: (a) an expression/partial index keyed by target `event_id` and `rel_type` for the `rel_type` relations — edits/reactions/threads (generalizing the thread index sketched in ADR 0017); and (b) a separate index on the **nested reply target**, `relates_to->'m.in_reply_to'->>'event_id'`, since replies carry no `rel_type`. Both make "all relations pointing at event X" an indexed lookup rather than a window scan, and both apply retroactively to already-stored rows — additive, backfill-free.
- Store reads, all scoped by `(account_id, …)`:
  - **Edits (`m.replace`).** Resolve the latest edit per target by `origin_ts`; surface the replaced content plus edit metadata (`edited`, `edit_count`, `latest_edit_ts`). The raw edit events stay on disk (append-mostly; provenance and original ciphertext preserved — the same philosophy as redaction masking). The timeline read _collapses_ them into the target rather than emitting standalone edit rows. matrix-rust-sdk has relation-aggregation support we can lean on, but the durable resolution is a store concern so it holds for events outside any client window.
  - **Reactions (`m.annotation`).** Group by target, then by `key`; per-emoji counts (plus the senders, for "did I react / who reacted").
  - **Replies (`m.in_reply_to`).** Direct replies to an event — matched on the nested `m.relates_to.m.in_reply_to.event_id` target (no `rel_type`).
  - **Threads (`m.thread`).** Thread membership; a per-thread summary (root event + latest reply + reply count) and a thread-scoped timeline read (reuse the M5 cursor pagination, scoped to a thread root).
- **Validity & resolution rules.** Aggregating "all relations by target" is necessary but not sufficient — the resolver must enforce what homeservers don't (ADR 0021 notes edit authorship is unenforced upstream):
  - **Edit authorship + type.** An `m.replace` is honored only when its sender equals the _original_ event's sender; edits from anyone else are dropped. An edit whose replacement `msgtype` is incompatible with the target is dropped.
  - **Redaction.** A redacted edit or reaction stops contributing (the latest _non-redacted_ edit wins; a redacted reaction leaves the tally). A redacted reply / thread member / thread root is still counted for structure (membership, reply count) but presented with masked content, so a redacted root doesn't make its thread vanish.
  - **Duplicate reactions.** The same `(sender, key)` counts once no matter how many annotation events arrived.
  - **Deterministic latest edit.** Resolved by `origin_ts`, **tie-broken by a stable key** (the store's monotonic insertion id, then `event_id`) so the winner is deterministic when timestamps collide — never "whichever the query happened to return."
- Computed at read time for MVP — the indexes make it cheap at Riley scale. Incremental materialization (maintaining tallies on ingest) is a later optimization, not a re-architecture.

**Verification (8a):** Seed a room with a message, then add a reaction and an edit _far outside_ the default timeline window; store-layer reads return the correct per-emoji count and the edited body regardless of window position; thread and reply lookups resolve over rows stored _before_ this milestone (proving the backfill-free claim). Add regression cases for the validity rules: an edit from a non-sender is ignored; a redacted edit reverts to the prior body and a redacted reaction leaves the tally; a duplicate `(sender, key)` reaction counts once; two edits with equal `origin_ts` resolve deterministically; and a reply is found via its nested `m.in_reply_to` target.

#### 8b. Aggregation API endpoints

Account-nested per the M5a convention:

- `GET /v1/accounts/{account_id}/events/{event_id}/reactions` — per-emoji tallies (issue #22 Option A): `{ "👍": { "count": 2, "me": true, "senders": […] }, "❤️": { … } }`.
- `GET /v1/accounts/{account_id}/events/{event_id}/replies` — direct replies to an event.
- `GET /v1/accounts/{account_id}/rooms/{room_id}/threads` — thread list (root + latest reply + reply count).
- `GET /v1/accounts/{account_id}/rooms/{room_id}/threads/{root_id}/timeline` — thread-scoped timeline, reverse-chronological with the same cursor pagination as the room timeline.
- **The M5 timeline read now returns aggregated events:** the latest edited body in place, standalone edit events stripped, plus a per-event `reactions` summary and `edited` / `edit_count` fields on `EventDto` (issue #22 Option B). An optional `GET …/events/{event_id}/edits` exposes the forensic edit history.
- WS (`/v1/ws`): raw relation events keep flowing live so clients can apply deltas, but the aggregation endpoints and the `EventDto` fields are the authoritative resolved view. Dedicated aggregation-update WS frames (a delivered tally delta) are a later add, not MVP.

**Verification (8b):** `GET …/reactions` returns grouped counts for a message whose reactions arrived in a later page; the timeline read shows the latest edited body with no stray edit rows; `GET …/threads` lists a thread with the correct reply count and its scoped timeline returns only that thread's events, reverse-chronological with stable pagination; an edit / reaction / reply sent over M6 round-trips and shows up aggregated.

### 9. Search

The Tantivy index. It runs after aggregation (M8) — so the text it indexes is the _latest_ edited body, not a superseded one — and **before** backfill (M10), deliberately: with the index in place first, the deep history that backfill pages in is indexed **incrementally as it arrives**, in the same single pass that stores it, rather than needing a second bulk sweep over the corpus.

- `axon-search` opens a Tantivy index.
- Schema fields: `event_id`, `account_id` (facet), `room_id` (facet), `sender` (facet), `origin_ts` (date), `body` (text).
- `body` analyzer chain: default tokenizer + `LowerCaser` + `AsciiFoldingFilter` + `Stemmer` (English). All built-in Tantivy token filters — register the analyzer once and reference it from the field schema.
- Populate on event ingestion in the shared pipeline — so everything that path ingests (live sync, **and the M10 backfill that lands after this milestone**) is indexed as it arrives. Sequencing search before backfill is what makes backfilled history index incrementally instead of being re-read in a separate pass.
- **Initial index build (one-time).** At this milestone the `events` table already holds the live-synced slice (M3+) that predates the index; a bulk pass streams those existing rows (batched, ordered) in so they're covered too. It runs on first boot after search is enabled, gated by an index-built marker (search-index metadata / a `search_index_built` flag) so it does not repeat, and is also exposed as an `axon search reindex` CLI subcommand for schema-change rebuilds. The index is derived data keyed by `event_id`, so the pass is idempotent and a from-scratch rebuild is always safe. Because backfill comes _later_ and indexes incrementally, this one-time build only ever covers the comparatively small pre-backfill slice — not the deep history.
- **Index from the resolved projection, not the raw event — so ingestion order can't corrupt the index.** Backfill pages newest→oldest, so a relation can arrive _before_ its target (an edit or redaction indexed, then the older target lands later) and naively re-index stale or already-redacted text. The rule that makes this order-independent: a target's search document is always (re)derived from the store's **resolved M8 projection** (latest non-redacted body + redaction state) at index time, never from a relation event in isolation. Concretely: indexing an `m.replace` / `m.annotation` / `m.room.redaction` (re)derives and writes the _target's_ doc; indexing a message derives its own doc from the projection (which already folds in any relations that arrived first). A redaction removes the target doc; an edit rewrites it to the latest body. A lightweight reconciliation pass closes any window where a relation was applied before its target existed.
- `GET /v1/search?q=…&account_id=…&room_id=…&sender=…&from=…&to=…`.
- BM25 ranking; paginated.
- No fuzzy/typo, synonym, or semantic search in MVP (see tech-spec search section). If a bounded fuzzy mode is wanted later, it's a query-time `FuzzyTermQuery` toggle on this endpoint, not an analyzer change.

**Verification:** Index a known corpus (e.g. dump 1000 events from a test room); assert an exact phrase query returns the expected top hit; confirm case- and diacritic-insensitivity (`cafe` matches `café`) and plural matching (`cat` matches `cats`); confirm an **edited** message is found by its new text and not its old; latency p95 under 200ms on the Riley-shape target. Cover the **out-of-order ingestion** cases backfill produces: ingesting an edit _before_ its target, then the target, leaves the index showing the edited body (not the stale one); ingesting a redaction _before_ its target leaves the target unsearchable once it arrives. (That a phrase from a **backfilled** pre-install message becomes searchable is asserted in M10, once backfill has streamed it through the now-existing index.)

### 10. History backfill

Sync alone only ingests events _going forward_ (plus the shallow `sync.timeline_limit` window on a room's first sync — ADR 0015). Backfill is the engine that reaches back for a room's pre-existing history, so the timeline read, the M8 aggregations, and the M9 search index cover more than the post-install slice. `recover()` (M7a) imports the _keys_ to decrypt old messages; it does not fetch the _messages_ — those must be paged from the room. (ADR 0018.) Backfill runs **last** of the read-side milestones on purpose: by now the shared ingestion path already feeds M8 aggregation and the M9 search index, so every event backfill pulls is aggregated and indexed incrementally as it streams in — one pass over the deep history, no separate bulk reindex. Together with sync this is what satisfies the PRD's full-history success criterion and the 100–200k-event working-set target.

- A bounded, **resumable** engine that pages backward through each room's timeline via the SDK's room pagination (`/messages`), decrypting with the keys already imported by `recover()` / gossip (M7a), and persisting through the **same ingestion path** as live sync — so hot columns, crypto siblings, redaction handling, the M8 aggregation indexes, and the M9 search index all apply uniformly, and re-runs are idempotent (`ON CONFLICT DO NOTHING`).
- Per-room backfill state (e.g. a `room_backfill` table: `(account_id, room_id, oldest_seen_token, complete, updated_at)`) so progress survives restarts and the engine knows where to resume and when a room is exhausted.
- Background + throttled: rate-limited so it never starves live sync; configurable target depth (a bounded number of events/days, or "to room start").
- This retires the `sync.timeline_limit` bump as the "bounded substitute" for real backfill (ADR 0015).

**Verification:** Point `axon` at a room with substantial pre-existing history; confirm the stored event count climbs toward the room's full history rather than the initial window; confirm backfilled _encrypted_ events decrypt (keys via `recover()`); confirm a **search** for a phrase from a pre-install message now returns it (proving incremental indexing during backfill); kill and restart `axon` mid-backfill and confirm it resumes without duplicating rows.

### 11. Media proxy

- `axon-media` resolves MXC URLs against the upstream homeserver for the relevant account.
- Bounded LRU cache on local disk (size configurable; default 5GB).
- `GET /v1/media/{account_id}/{server}/{media_id}` with proper caching headers and range-request support.
- No S3 backend. Do not add one. Off-host/durable media is already solved at the homeserver layer (e.g. [`synapse-s3-storage-provider`](https://github.com/matrix-org/synapse-s3-storage-provider)); there's no case to reinvent it in the agent, whose cache is a bounded LRU with the homeserver as source of truth.

**Verification:** Send a message with an image attachment, fetch the image through `/v1/media/…`, confirm it renders inline in `axon-tui` (or curl the URL and inspect the bytes). Fill the cache past its limit and confirm LRU eviction.

### 12. Drafts and per-device read state

- Tables: `device_state` keyed by `(account_id, device_id, namespace, key)` with an opaque value blob and `updated_at`.
- Devices are identified by a client-supplied UUID at first registration.
- Endpoints: `GET/PUT /v1/devices/{device_id}/state/{namespace}`.
- Live sync via WS: changes broadcast to other devices owned by the same human; last-write-wins by `updated_at`.

**Verification:** Two `axon-tui` instances (acting as separate devices); typing a draft in one updates the other within a second.

### 13. Deployment docs

- `docs/self-hosting.md` covering:
  - Prerequisites (Postgres, Synapse / Dendrite accessible).
  - Build / install (Cargo + Docker options).
  - Config reference (every setting from `axon-core`'s config loader).
  - First-run flow: account provisioning via `POST /v1/accounts/login`, device verification (`POST …/verify` or `…/recover`), token minting, running `axon-tui`.
  - Operational basics: backups (`pg_dump` + media cache directory + the per-account SDK store dirs under `data_dir/`), upgrades, logs.
- **Release automation:** a GitHub Actions workflow that builds tagged-release single-binary artifacts for the common platforms (Linux x86_64/aarch64, macOS arm64), so the install story is "download one binary and run it" — no toolchain required. This is what makes the single-binary premise real for a non-Rust operator.
  - Deployment recipes — at minimum one each for:
    - Localhost / same device as your client (the simplest case, and a perfectly normal production setup for one person): `axon` + Postgres on your own machine, `axon-tui` connecting over `localhost`. This is also the dev and first-run path.
    - Home machine behind a private mesh VPN (the recommended multi-device self-host path). axon + Postgres on hardware you own — the box under your desk, a home server, a NAS — reached from your other devices over a private network such as Tailscale, with **no port ever exposed to the public internet**. This best fits axon's premise: your data stays on your hardware. It also pairs with the M7b token auth as defense-in-depth — the VPN is the network gate, the token is the application gate. The two are complementary, not substitutes: the VPN never stands in for app auth on the destructive/secret lifecycle endpoints, which stay **loopback-bound until 7b** (see M7a) so they are never remotely reachable without the token regardless of the network. The VPN's job is to keep the read/mutation surface off the public internet, not to retroactively gate those endpoints.
    - Railway (or a similar Procfile-style PaaS).
    - DigitalOcean droplet (Docker Compose + nginx reverse proxy + Let's Encrypt).
    - AWS (EC2 + RDS Postgres; ECS optional; reference Terraform welcome but not required).
    - Bare Linux VPS (covered in the operational basics above).

**Verification:** A reader who has not touched the codebase follows the doc top to bottom on a fresh VM and reaches the "daily-driver through `axon-tui`" PRD success criterion. At least one deployment recipe is exercised end-to-end (any of them) by someone other than the author.

## Milestone resequencing (post-M6)

Milestones 1–6 shipped as originally numbered. After M6, the sequence was rethought to reflect what the project actually needs next — a real account lifecycle, server-side relation aggregation, and a terminal client in place of the planned web alpha. The current plan (this document) supersedes the original M7–M13 ordering, and has itself been extended twice more since (M15–M19 added; the web client revived as a second, parallel client track). Expect further resequencing before MVP ships — this table is a living bridge, not a final numbering.

| Now                                                                  | Was                                                            | Change                                                                                                                                                                                                                                                                                                                   | Status                                                                                                                                                                                                                                                          |
| -------------------------------------------------------------------- | -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **7a** Homeserver account lifecycle & verification                   | — (new) + old "M5c"                                            | Login / verify / recover / logout / delete as a first-class API; closes the interactive verification deferred from M5 and GH issues #14, #24.                                                                                                                                                                            | Shipped                                                                                                                                                                                                                                                         |
| **7b** Client ↔ axon bearer-token auth                               | old M8 (Auth)                                                  | Unchanged in substance; renumbered.                                                                                                                                                                                                                                                                                      | Shipped                                                                                                                                                                                                                                                         |
| **7c** Sender-device trust & content authentication                  | — (new)                                                        | Evaluate + expose whether _other_ senders' devices are cross-signed; delivers the verification-bundle API the tech-spec already promises.                                                                                                                                                                                | Shipped                                                                                                                                                                                                                                                         |
| **8** Relation aggregation (8a backend, 8b API)                      | — (new), subsumes old M13 (Threads)                            | Edits / reactions / replies / threads aggregated server-side (GH issue #22). Threads are the `m.thread` case, no longer a standalone milestone.                                                                                                                                                                          | Shipped                                                                                                                                                                                                                                                         |
| **9** Search                                                         | old M9a                                                        | Runs after aggregation (indexes latest bodies) and **before** backfill, so backfilled history indexes incrementally.                                                                                                                                                                                                     | Shipped                                                                                                                                                                                                                                                         |
| **10** History backfill                                              | old M9b                                                        | Now runs last of the read-side milestones, so it aggregates + indexes incrementally in one pass over the deep history.                                                                                                                                                                                                   | Shipped                                                                                                                                                                                                                                                         |
| **11** Media proxy                                                   | old M7                                                         | Unchanged in substance; renumbered.                                                                                                                                                                                                                                                                                      | Shipped                                                                                                                                                                                                                                                         |
| **12** Drafts & per-device read state                                | old M10                                                        | Unchanged in substance; renumbered.                                                                                                                                                                                                                                                                                      | Partial — drafts shipped; cross-device read markers were reverted (`116b3cb`) and later re-landed on the TUI side (PR 217); confirm current status against `docs/client-parity.md` before relying on it                                                         |
| **13** Deployment docs                                               | old M12                                                        | Retargeted at `axon-tui`; first-run flow now uses the M7a account endpoints.                                                                                                                                                                                                                                             | Partial — `axon init`, platform dirs, and a full Docker Compose stack (`deploy/`, ADR 0052) shipped; `docs/self-hosting.md` and the non-Docker deployment recipes (Railway, DigitalOcean, AWS, bare VPS) are still unwritten. **This is the current MVP gate.** |
| **14** OAuth authorization server                                    | — (new)                                                        | Axon becomes its own minimal OAuth 2.0 AS + OIDC relying party (Google/Microsoft), ADR 0054. Pulled forward from the post-MVP roadmap in `tech-spec.md`.                                                                                                                                                                 | Shipped (Apple deferred to iOS client work)                                                                                                                                                                                                                     |
| **15** Media send/upload                                             | — (new)                                                        | Adds the write-side counterpart to M11: staged uploads plus `m.image` / `m.file` send over Axon's `/v1/` API.                                                                                                                                                                                                            | Shipped (server); client UX followed on both `axon-tui` (ADR 0061) and `axon-web` (ADR 0065)                                                                                                                                                                    |
| **16** Device-list / discovery endpoint                              | — (new)                                                        | `GET /v1/accounts/{account_id}/devices`, closing ADR 0053 item 2 so a client can build a real SAS device picker instead of a blind id.                                                                                                                                                                                   | Shipped (server); no client picker UI yet on either `axon-tui` or `axon-web`                                                                                                                                                                                    |
| **17** Media thumbnail proxy                                         | — (new)                                                        | `GET .../media/{...}/thumbnail`, ADR 0063.                                                                                                                                                                                                                                                                               | Shipped                                                                                                                                                                                                                                                         |
| **18** Live-event ephemeral passthrough                              | — (new)                                                        | `m.typing`/`m.receipt` forwarded verbatim over `/v1/ws`, ADR 0056.                                                                                                                                                                                                                                                       | Shipped                                                                                                                                                                                                                                                         |
| **19** Matrix C-S verb batching (typing/membership/settings/profile) | — (new)                                                        | Closes the remaining Matrix C-S parity gaps inventoried in issue #279, stamping ADR 0021's consumer-owned-port pattern once per trait group instead of once per verb; six PRs (M19a–M19f), see ADR 0068.                                                                                                                 | Shipped (all six PRs, server-side); client UI for room settings/power levels/account actions is separate follow-up work not yet started on either client                                                                                                        |
| _(revived, not dropped)_ Web client                                  | old M11 (Web alpha, originally dropped in favor of `axon-tui`) | Reinstated as a second, parallel reference client rather than staying a "credible later addition" — see ADR 0031 (client strategy) and ADR 0046 (framework pick + roadmap). It runs its own `M-Wn` milestone series alongside the numbered sequence above, tracked in `docs/client-parity.md` rather than in this table. | Active, not yet at parity with `axon-tui` — see `docs/client-parity.md`                                                                                                                                                                                         |

Older docs (`AGENTS.md` "Current state", the ADR log) still reference the original numbers as historical context; this table is the bridge. References to milestone numbers in those frozen/append-only docs are not retro-renumbered.

## Open decisions that gate milestones

The threads question carried over from [`tech-spec.md`](./tech-spec.md) is now resolved:

- **Threads — resolved: folded into M8 (Relation aggregation), in MVP.** Rather than a dedicated post-MVP milestone, threads ship as the `m.thread` case of the M8 aggregation machinery. The store captures `m.relates_to` generically (incl. `m.thread`) in `events.relates_to` (ADR 0015), so this is additive and backfill-free — the thread membership of every already-stored event is recoverable from data on disk. See Milestone 8.

Everything else is settled. If an ambiguity arises during implementation that neither the PRD nor the tech spec covers, stop and ask rather than picking silently.

## Conventions

Follow Matrix OSS community conventions first; fall back to standard Rust conventions where Matrix doesn't speak to the question. Match `matrix-rust-sdk`'s style and naming where there is overlap (event types in `snake_case` like the Matrix spec, room/event identifiers as opaque strings, error enums per crate with `thiserror`).

- **Crate names.** `matrix-axon-*` on crates.io if we ever publish; internal workspace paths `crates/axon-*`. Binary name `axon`.
- **Migrations.** Under `crates/axon-store/migrations/`; numeric prefix; sqlx migrate.
- **Provenance.** All decrypted content rows include `account_id` and `provenance` (default `upstream_homeserver`) and link to original ciphertext rows.
- **Account scoping.** Every account-scoped table — rooms, events, room state, account data, device keys, drafts, read state, search index docs — carries `account_id` from day one. Cross-account aggregation happens in the API layer, not the store layer.
- **API routes.** All HTTP under `/v1/…`. WebSocket at `/v1/ws`. Envelope `{type, account_id, payload}` on every WS message.
- **OpenAPI.** The spec is the source of truth. Handler types must compile against it (utoipa). Drift between the spec and generated client stubs is a bug.
- **Errors.** `axon-core` defines the top-level error enum; crates re-export their own narrower errors that convert into it. Use `thiserror` for definitions and `anyhow` only at binary boundaries.
- **Logging.** `tracing` with structured fields including `account_id`, `room_id`, `event_id` where applicable. Match `matrix-rust-sdk`'s span layout where the two libraries are in the same call path.

## Verification per milestone (end-to-end, not just `cargo check`)

- **Sync milestones (3, 4).** Point at a Synapse-in-docker, watch decrypted rows accumulate in Postgres; query a known room's timeline by SQL; confirm redactions mask content.
- **Account-lifecycle milestone (7a).** Login a second account at runtime; verify the device (interactive SAS over `/v1/ws`, or recovery-key); logout and confirm it deactivates with its archive retained and a fresh login reactivates it; delete and confirm the DB rows and SDK store dir are gone; confirm a deactivated account neither syncs nor sends.
- **Sender-trust milestone (7c).** In an E2EE room, confirm `sender_trust` on timeline events distinguishes cross-signed from unverified senders, the verification-bundle endpoint returns per-event evidence, and a sender identity change surfaces as `verification_violation`.
- **API milestones (5, 6, 8b, 11, 12).** curl against the running server; assert aggregated reads (reactions/threads/edits) resolve outside the timeline window.
- **Auth milestone (7b).** End-to-end: mint, use, revoke, confirm rejection.
- **Search milestone (9).** Index a known corpus, assert top results (including edited messages); measure p95 against the Riley-shape target.
- **Backfill milestone (10).** Deep-history room: stored count climbs toward full history and a pre-install phrase becomes searchable (incremental indexing); restart mid-backfill resumes without duplicates.
- **Deployment docs (13).** A reader follows the doc on a fresh VM and reaches the daily-driver success criterion in under an hour; at least one cloud recipe is exercised end-to-end.

## Documentation for agentic contributors

The OpenAPI spec covers the wire protocol but not the codebase. Future coding agents (Claude Code, Codex, Cursor, whatever comes next) need a separate set of in-repo docs to understand structure, intent, and non-obvious decisions. We maintain four:

### 1. `AGENTS.md` (canonical) + `CLAUDE.md` (pointer)

`AGENTS.md` at the repository root is the vendor-neutral orientation doc that most coding agents now look for by convention. `CLAUDE.md` is a one-line pointer to `AGENTS.md` so Claude Code finds it without us maintaining two copies.

- **Create** `AGENTS.md` during Milestone 1. Initial contents: project name, one-paragraph summary, pointer to `docs/mvp/`, the directory tree from "Project layout" above, a short conventions section that links to the "Conventions" section of this doc, and a "Current state" section that records which milestone is in flight.
- **Create** `CLAUDE.md` during Milestone 1 with one line: `See AGENTS.md.`
- **Update** `AGENTS.md` as you go. After every milestone, revise the "Current state" section and append any non-obvious design choices made during that milestone — library picks, schema details that aren't in the specs, build steps, gotchas. The next agent shouldn't have to reverse-engineer those.
- **Keep it short.** `AGENTS.md` is a high-density orientation, not a wiki. If a section grows past a page, break it out into a dedicated doc under `docs/` and leave a one-liner pointer behind.
- **Treat it as code.** Edits go through the same PR review as code changes.

Goal: any agentic contributor opening this repo cold reads `AGENTS.md` and is productive within minutes, without having to grep around or re-read the MVP specs.

### 2. Per-crate `README.md` + crate-level `//!` rustdoc

Every crate under `crates/` has a `README.md` and a crate-level doc comment (`//!`) in `lib.rs`. They serve different audiences but cover the same ground:

- **What this crate is responsible for** in one sentence.
- **Public API surface** at a glance — the main types and entry points.
- **Dependencies it owns** vs. dependencies it consumes (e.g. `axon-store` owns Postgres connections; `axon-api` consumes a `Store` handle).
- **Anything load-bearing that isn't obvious** from the code — invariants, "do not call this from inside a sync handler," etc.

Rustdoc renders for human-readable browsing on docs.rs (if we publish) and for `cargo doc --open` locally. The `README.md` is what an agent or human reads first when grepping by file. Keep them consistent; if they drift, the README is the source of truth and rustdoc is regenerated to match.

### 3. Architecture decision records under `docs/adr/`

Lightweight ADRs (Michael Nygard format — Context / Decision / Consequences, one page max) capture non-obvious decisions as they're made. Filename pattern: `NNNN-kebab-case-title.md`, monotonically numbered.

Write an ADR when:

- You pick one library over another for a non-trivial reason.
- You make a schema or API choice the specs don't prescribe.
- You discover an upstream bug or quirk and work around it.
- You decide _not_ to do something that seems like an obvious next step.

The first ADR (`0001-record-architecture-decisions.md`) is the meta-ADR adopting the practice — created in Milestone 1.

Don't write ADRs for decisions the MVP specs already settle; those are anchored in `docs/mvp/` and re-stating them in ADRs creates drift. The ADR directory is for what happens _during_ implementation that the specs don't cover.

### 4. `docs/mvp/` (this directory, locked at end of MVP)

PRD, tech spec, and implementation spec freeze at MVP ship. After that, they become historical reference — changes to product/architecture go through new docs (or new versions). An agent reading them later should treat them as "what we decided going in," not "current state."

The current state of the system lives in `AGENTS.md` and the ADR log.

## References and test corpus

We're not the first project in this space. Other Matrix clients have already hit the protocol's sharp edges; their issue trackers are a cheap source of edge cases we should cover from the start.

### Architectural references

Read these for lessons, not to copy code.

- **gomuks.** Closest architectural cousin — persistent backend, thin frontend, similar problem framing. Differences: single-user / single-account, no documented API, mautrix-go instead of matrix-rust-sdk. **Transfers:** protocol handling, sync edge cases, room-state management, redaction / edit semantics, what a server-side Matrix client actually has to do. **Doesn't transfer:** anything API-shaped or multi-account-scoped.
- **Element X / matrix-rust-sdk.** Same crypto and sync library we use. Their issue tracker is where library-level bugs surface first. **Transfers:** library gotchas, sliding-sync edge cases, megolm session handling, key-backup recovery flows. **Doesn't transfer:** their client-side state model — we own that on the server.
- **mautrix bridges** (mautrix-telegram, mautrix-discord, mautrix-whatsapp, etc.). Source of unusual bridged event shapes. Their issues reveal what real bridge traffic looks like and where bridges produce content our timeline rendering needs to tolerate.
- **Synapse / Dendrite issues.** Where homeserver-side quirks we have to tolerate get discussed (rate limits, sync response oddities, MSC4186 compliance gaps).

### Test corpus

A `tests/fixtures/` directory holds JSON event payloads, recorded sync responses, and end-to-end scenarios that drive the protocol-level test suite. Build it up by harvesting categories from the trackers above and add fixtures as new edge cases appear (whether discovered locally or upstream).

Categories to cover, all with at least one fixture before MVP ships:

- Gappy backfill (sliding sync delivering a window with gaps in the timeline).
- Megolm session loss → undecryptable events (UTDs) → key-backup recovery once keys arrive.
- Redaction edge cases: redaction of an edit, redaction of a redaction, redacted-while-decrypting.
- Room upgrades mid-conversation (`m.room.tombstone` arrives during active reading).
- Large rooms (10k+ members, multi-MB state events).
- Slow / flaky upstream homeservers (timeouts, partial sync responses, retry behavior).
- Bridged event shapes (mautrix `m.room.message` variants, bridge-specific `body` and `formatted_body` content, reply-to chains across the bridge).
- Malformed or unexpected events from upstream (we ignore-and-log, don't crash).

Treat this as a living checklist: when an upstream issue closes with "fixed bug in X edge case" and X is one we'd plausibly see, add the fixture for X if we don't have it.

## What not to build

Mirrors the PRD non-goals and out-of-scope items; the agent should not drift into these:

- No push code paths (no APNs, FCM, web push). The event-emit surface is designed to accept a router later; do not build the router.
- No multi-human-per-process isolation. One human per Axon.
- No federation hooks, no peer-to-peer ingestion.
- No native client scaffolding (iOS, desktop). Generated Swift stubs only.
- No admin API.
- No bridge metadata normalization.
- No importers from existing clients.
- No full OAuth 2.0 server. Bearer tokens via CLI only. _(Post-MVP, shipped ahead of MVP freeze: M14 landed — axon is its own minimal OAuth 2.0 authorization server plus an OIDC relying party to Google/Microsoft, behind the existing `TokenVerifier` seam, ADR 0054. `axon oauth bind` connects an identity; Apple/Sign-in-with-Apple stays deferred to the iOS client work.)_
- No advanced search UI (faceted, semantic). Backend ships; a minimal search input in `axon-tui` ships; rich UI does not.
- No S3 / object-store media backend. Local disk LRU cache only.
- No client-side media-send UX in MVP docs. _(Post-MVP: shipped. M15 / ADR 0059 landed the server side — staged uploads plus `m.image` / `m.file` mutations. The client UX followed: `axon-tui`'s `/send` (ADR 0061) and the web client's picker/drop/paste (ADR 0065, M-W8.5).)_
- No spaces-specific endpoints. Events flow through.
- No `store_key` rotation. One key decrypts every account's token; rotation stays deferred (ADR 0008), tracked against #24.
- No incremental/materialized aggregation tallies in MVP. M8 aggregates at read time over indexed relations; maintaining counters on ingest is a later optimization.
