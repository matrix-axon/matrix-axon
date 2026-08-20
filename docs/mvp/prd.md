# Axon MVP — Product Requirements

Related docs: [`tech-spec.md`](./tech-spec.md), [`implementation.md`](./implementation.md).

> **Status: MVP has not shipped.** This document freezes at MVP ship per `implementation.md`'s "Documentation for agentic contributors" convention; until then it's still a living plan. See `implementation.md`'s status banner and milestone-resequencing table for what has actually landed and what's still gating MVP (currently: deployment docs, `docs/self-hosting.md`).

## Project

**Axon** is a self-hosted **BFF (backend-for-frontend)** for Matrix: a persistent state layer that sits between a user's homeserver(s) and their clients, holding the persistent state, search index, and per-device coherence that Matrix clients otherwise reinvent themselves. Clients consume it through a documented, versioned HTTP + WebSocket API.

> **On "agent."** We sometimes call Axon a "personal agent," but in the [back-end-for-front-end](https://philcalcado.com/2015/09/18/the_back_end_for_front_end_pattern_bff.html) / ZNC-bouncer sense — a passive proxy that runs on your behalf — **not** in the autonomous-AI-agent sense. There is no model or autonomy in the loop; Axon does only what its one human owner tells it. Where the AI connotation risks confusion, prefer "BFF," "bouncer," or "proxy."

**Naming convention:**

- Rust crates: `matrix-axon-*` prefix (e.g. `matrix-axon-store`).
- Internal workspace path: `crates/axon-*`.
- Binary / product: `axon`.
- Alpha client: `axon-tui` (terminal client).

`axon` is a working title. Naming feedback welcome on this PR.

## One-line pitch

The personal agent layer Matrix is missing — what mail has had for decades — designed to be self-hosted and consumed by arbitrary clients through a stable API.

## Problem & opportunity

Matrix's client-server architecture pushes a lot of work onto clients: continuous sync, E2EE key management, local indexing, push handling, per-device state. This works for desktop clients with persistent connections and generous storage. On mobile and across devices it falls apart:

- Initial sync is slow because the phone fetches and decrypts hundreds of thousands of events.
- Local storage grows unboundedly as media accumulates.
- Search either lives only on-device (rebuilt per install, no cross-device) or doesn't exist.
- Push notifications either carry no content (useless) or require the phone to wake and decrypt (battery-hostile and brittle).
- Multi-device state beyond read receipts (drafts, scroll position, mutes) doesn't sync.
- Reinstalling a client starts from scratch.

Existing tools cover parts of this but not cleanly:

- **Element X / matrix-rust-sdk clients** push everything client-side.
- **gomuks** has the right shape but is single-user, single-account, no documented API, designed for its own UI.
- **Pantalaimon** decrypts only.
- **Beeper** solves it but is a commercial hosted service.

The conceptual gap: Matrix has no analogue of an IMAP / SMTP server combined with a personal search index — a hosted persistent state layer that arbitrary clients can consume.

## Target user for MVP

**Riley** is the persona we're designing for. Riley is:

- A developer or technical operator — comfortable on the command line, has self-hosted services before.
- 10–50 active Matrix rooms, 100–200k events of history.
- Mostly E2EE rooms.
- Currently uses one or two Matrix accounts (work + personal is common).
- Reads primarily on mobile, occasionally on desktop, and wants the two to stay coherent.

The MVP must let Riley self-host Axon, point it at their homeserver, and use `axon-tui` as their primary read / reply surface.

## MVP product scope

### Axon (the agent)

- **One process per human.** Each Axon instance has a single trust boundary: a single human owner.
- **Multiple Matrix accounts inside one Axon.** The agent supports N accounts (e.g. personal + work, across different homeservers) under that single owner. The data model and code path are N-account from day one; accounts are managed at runtime through an account-lifecycle API (login / verify / recover / logout / delete) rather than only at boot from config.
- **Account lifecycle.** Each Matrix account has an explicit lifecycle: log in to a homeserver, verify the Axon device (interactive SAS or recovery-key), log out (which stops the account but **retains** its archive — the point of a persistent state layer), and delete (which removes all of that account's data from Axon). Only active accounts sync and send; there is no path for a stale, deconfigured account to keep acting.
- **Sync.** Simplified Sliding Sync (MSC4186) against Synapse and Dendrite. Legacy `/sync` is not supported.
- **E2EE.** olm / megolm, server-side key backup, device verification (interactive SAS and recovery-key) exposed through the account-lifecycle API, and **sender-device trust** — every event carries whether its sending device was cross-signed, so clients can flag messages from unverified senders.
- **Event store.** Postgres-backed. Decrypted timelines, room state, account data, original ciphertext, megolm metadata, sender device keys preserved for later verification.
- **Search.** Tantivy index populated on event ingestion. BM25 ranking, room / sender / account filters. Exposed via the API and exercised by a minimal search input in `axon-tui`.
- **Media proxy.** Fetches upstream MXC URLs and caches to local disk (bounded LRU). Cache exists so the phone fetches over the LAN once Axon has the bytes and so a media-deleting homeserver isn't the only copy. S3-compatible backend is out of scope for MVP — added later if hosted-Axon deployments need durable media storage.
- **Drafts and per-device read state.** Opaque blobs scoped to `(account, device, namespace)`, synced over WS.
- **Local API.** REST over `/v1/`, WebSocket at `/v1/ws`. OpenAPI 3.1 spec is the source of truth; `axon-tui` consumes the Rust API types directly, Swift stubs are generated for the deferred iOS client, and TypeScript stubs remain available for a future web client.
- **Auth.** Two layers. Client ↔ Axon: long-lived bearer tokens issued via an `axon` CLI subcommand, per-device-name and revocable (full OAuth 2.0 + PKCE deferred post-MVP). Axon ↔ homeserver: per-account Matrix login with device verification, managed through the account-lifecycle API.
- **Relation aggregation.** Edits, reactions, replies, and threads are aggregated server-side, so clients get resolved views (latest edited body, per-emoji reaction tallies, reply and thread listings) regardless of how their timeline pagination falls — not partial counts that depend on scroll position.
- **Deployment.** Single Rust binary plus docker-compose for Postgres. Self-hosting guide bundled. The default setup is one `axon` process on one host (typically `http://localhost:PORT`); `axon-tui` is a separate terminal binary that connects to it.

### `axon-tui` (the alpha client)

A terminal client (Rust) that exercises the agent API end-to-end. It is the reference client for the MVP — the "alpha" tag describes its polish, not its long-term role — and it replaced the originally-planned `axon-web` (React) web app. The product is the agent and its API; `axon-tui` is the integration surface that proves the API is usable.

- Rooms list, sorted by most recent activity.
- Room view: timeline reverse-chronological by default; infinite scroll backwards for history.
- Composer: send, edit, redact, react.
- Inline media viewing through the agent media proxy.
- Minimal search input that hits `/v1/search`, scoped to the active room or all rooms.
- Auth: paste a token minted by the agent CLI.

It does not need to be polished. It exists to validate the API surface and to give Riley a usable client on day one.

> **Sequencing update.** The web client described here as "a credible later addition" was pulled forward and is now under active, parallel development (`axon-web`, ADR 0031/0046) rather than waiting for post-MVP. This does not change the MVP success criteria below, which remain gated on `axon-tui` — the web client is additional work happening alongside MVP completion, not a substitute for it. See `docs/client-parity.md` for how far each client actually is.

## Out of scope for MVP

These are deliberately deferred. Many appear on the post-MVP roadmap.

- **Push notifications.** No APNs, no FCM, no web push. The event store and event-emit surface are designed so a push router can be added later without schema changes.
- **Spaces as first-class API resources.** Events flow through; no space-specific endpoints.
- **Voice / video signaling.**
- **Advanced search UI** (faceted, semantic). Backend ships, simple search input ships in `axon-tui`; rich UI does not.
- **Admin API.**
- **Backup / restore tooling.**
- **Migration tooling from existing clients.** Onboarding is fresh sync only.
- **QR-code device verification.** Interactive SAS (emoji) verification and recovery-key device verification ship as a first-class account-lifecycle API (and `axon-tui` can drive them); QR-code verification is a follow-up.
- **Native desktop / iOS clients.** A reference iOS app is a separate future project; the MVP ships generated Swift stubs only.
- **Full OAuth 2.0 server.** Bearer tokens for MVP; OAuth + PKCE later. _(Post-MVP, shipped ahead of MVP freeze: M14 landed — axon is its own minimal OAuth 2.0 authorization server and an OIDC relying party to Google/Microsoft, ADR 0054. Bearer tokens remain the wire protocol; OAuth is an additional way to mint one.)_
- **Bridge metadata normalization.** Bridged events flow as ordinary Matrix events; clients render whatever the bridge places in event content.
- **Per-room or per-language search analyzers.** One default language-agnostic analyzer for MVP.
- **Durable media storage backend (S3-compatible).** MVP uses local disk cache only.

## Non-goals

These are not deferred — they are out of scope for the project as a whole, not just the MVP.

- **Not a homeserver.** Axon sits in front of an existing homeserver (Synapse, Dendrite, Conduit). Federation, room creation, and state resolution stay the homeserver's job.
- **Not a bridge.** Bridges live elsewhere. Axon treats bridged events as ordinary Matrix events.
- **Not a UI as the deliverable.** `axon-tui` exists as the reference client and as an integration test; the product is the agent and its API.
- **Not federated agents in v1.** A federated agent network is a credible v2/v3 direction but is not in scope here. Event provenance is captured in the schema so federation can be added later as an ingestion path, not a rewrite.
- **Not optimized for the no-E2EE case.** Plaintext rooms work fine, but the design assumes E2EE is the common case.
- **Not multi-human within a single Axon process.** Each Axon serves one human owner. SaaS-style isolation between different humans on shared infrastructure is not on the roadmap as a single-process feature; operators running Axon for multiple humans run one process per human.

## Success criteria

The MVP is done when:

1. **Initial sync.** A Riley-shaped account completes initial sync against Synapse on a residential connection within ten minutes.
2. **Search latency.** Full-history search returns relevant results in under 200ms p95, measured against the API directly and exercised through `axon-tui`'s search input.
3. **Reinstall.** A client built against the API can be wiped and reach full functionality within seconds of re-authenticating; no local backfill required.
4. **API documentation quality.** A developer who has not read the source can build a working client from the OpenAPI spec alone.
5. **Self-hosting.** A competent Linux user deploys Axon on their own hardware in under an hour using the self-hosting guide.
6. **Daily-driver client.** A Riley-shaped user reads and replies to their real Matrix rooms through `axon-tui` against a self-hosted Axon for a week without falling back to another client.

## Open product questions

- **Threads — resolved: in MVP.** Threads ship as part of server-side relation aggregation rather than a separate milestone or a post-MVP feature. The agent exposes thread-aware endpoints (`GET /v1/accounts/{id}/rooms/{room_id}/threads`, a thread-scoped timeline read) alongside reaction and reply aggregation; see the aggregation milestone in [`implementation.md`](./implementation.md).

See [`tech-spec.md`](./tech-spec.md) for implementation decisions that touch product behavior (auth, search analyzer, bridges, onboarding).
