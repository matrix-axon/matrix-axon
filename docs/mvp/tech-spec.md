# Axon MVP — Technical Specification

**Audience:** Engineers and reviewers with working Matrix and Rust literacy. Light on background; decisions and tradeoffs are the focus.

Related docs: [`prd.md`](./prd.md), [`implementation.md`](./implementation.md).

> **Status: MVP has not shipped.** This document freezes at MVP ship per `implementation.md`'s convention; until then, treat it as a living record — several items below (web client, OAuth) have already moved from "post-MVP roadmap" to "shipped ahead of freeze" as the sequencing evolved. See `implementation.md`'s status banner for current progress.

## Context & goals

Axon is the agent described in [`prd.md`](./prd.md): a self-hosted persistent state layer for one human's Matrix accounts, consumed by arbitrary clients (`axon-tui`, a terminal client, is the MVP reference client; a web client, `axon-web`, was pulled forward from the roadmap below and is now under active parallel development — see ADR 0031/0046; native mobile/desktop clients remain deferred) through a stable HTTP + WebSocket API.

This document records the architectural decisions for the MVP and the tradeoffs we weighed. It is not an implementation guide — that is [`implementation.md`](./implementation.md).

## Architecture overview

```
                 ┌─────────────────────────┐
                 │   Upstream              │
                 │   Homeserver(s)         │
                 │   (Synapse / Dendrite)  │
                 └────────────┬────────────┘
                              │ Matrix C-S API
                              │ (Simplified Sliding Sync,
                              │  media)
                              ▼
                 ┌──────────────────────────┐
                 │   Axon (single binary)   │
                 │                          │
                 │   axon-sync              │
                 │   axon-crypto            │
                 │   axon-store ──────── Postgres
                 │   axon-search ─────── Tantivy
                 │   axon-media ──────── disk cache
                 │   axon-api               │
                 └────────────┬─────────────┘
                              │ Axon API (REST + WS, /v1/)
                              ▼
                       ┌─────────────┐
                       │  axon-tui   │  (terminal client)
                       │  (deferred: │
                       │   web,      │
                       │   mobile)   │
                       └─────────────┘
```

**Trust boundary.** Axon is a Matrix device, not a homeserver extension. It is trusted by its single human owner the way a desktop client would be. Upstream homeservers do not trust it beyond what they trust any other client. Clients on the south side of Axon are trusted only as far as their per-device bearer tokens.

## Settled decisions

### Single binary + Postgres + local disk

One Rust binary (`axon`), one Postgres database (docker-compose reference deployment), media cached to local disk. No microservices. No required external dependencies beyond Postgres.

S3-compatible media storage is intentionally out of scope for MVP. The media proxy is a bounded LRU disk cache; the upstream homeserver remains the source of truth. Durable / off-host media storage gets revisited when there is a hosted Axon deployment or a multi-host operator that needs it.

### matrix-rust-sdk for sync and crypto

Sync, olm/megolm, key backup, cross-signing, and verification are delegated to matrix-rust-sdk. We do not reimplement any of it. Gaps discovered during implementation get upstreamed.

### Simplified Sliding Sync only

Sync uses Simplified Sliding Sync (MSC4186) only. No legacy `/sync` fallback. Tradeoff:

- **Win:** half the sync code path, half the test matrix, fits matrix-rust-sdk's preferred path.
- **Cost:** homeservers without SSS support are excluded. Synapse and Dendrite both ship it; the long tail of small homeservers is the excluded population.

We document the requirement and revisit if deployments hit a homeserver without SSS.

### Account model: one Axon per human, N Matrix accounts inside

A single Axon process serves a single human. That human may have N Matrix accounts (e.g. personal + work, across different homeservers) inside one Axon.

- Every account-scoped row carries an `account_id` foreign key.
- One matrix-rust-sdk `Client` and one crypto store per account.
- One combined Tantivy index with `account_id` as a facet field so unified search "just works" and per-account filtering is a query param.
- The WebSocket subscription delivers events from all accounts the human owns; every event carries its `account_id` in the envelope.
- The local API auth identifies the human (one token set per Axon); the human is authorized to act on any of their accounts.
- No cross-human isolation. Accounts inside one Axon share a process, a DB, and a filesystem because they belong to the same human.

Multi-human-within-one-process is a non-goal (see [`prd.md`](./prd.md)). Operators serving multiple humans run one Axon per human.

### Account lifecycle and active-account gating

Because the data model is N-account from day one, accounts need a real lifecycle rather than a one-shot config provision. Provisioning an account only from config strands the previous row when the config changes, and any row with a decryptable token would otherwise keep syncing and _sending_ — a stale, deconfigured account acting on the user's behalf. (Observed in practice; tracked in GH #14 and #24.)

- Each account carries an explicit lifecycle `state` (`active` / `deactivated`), orthogonal to verification status. The sync engine and the mutations gateway connect and serve **only `active` accounts** — never "anything with a stored token." `deactivated` is a reversible pause that retains data; this is not a soft-delete model.
- Accounts are added at runtime via an account-lifecycle API (`POST /v1/accounts/login`) rather than only at boot, so adding account #2…N never requires swapping config.
- Device verification is part of the lifecycle: interactive SAS (emoji) over `/v1/ws`, or recovery-key (4S) recovery that both imports the megolm backup and self-verifies the Axon device.
- Logout invalidates the upstream token and moves the account to `deactivated`, **retaining** its archive (a persistent state layer shouldn't discard history just because a device logged out); a fresh login reactivates it. Delete is the destructive path — it removes the DB rows (cascades) and the on-disk per-account SDK store dir, and a boot-time reconcile prunes orphan store dirs. Together they replace manual DB surgery.

The single `store_key` that encrypts every account's access token at rest is a known blast-radius concern; rotation stays deferred (ADR 0008) and is tracked against #24.

### Relation aggregation: server-side, read-time, over stored relations

Edits (`m.replace`), reactions (`m.annotation`), replies (`m.in_reply_to`), and threads (`m.thread`) are all _relation_ events. Axon stores them as ordinary events with the relation in the `relates_to` hot column. Leaving aggregation to clients breaks down because a client only holds a timeline window: a reaction or edit that lands outside that window (e.g. long after the original message) is silently missed, producing wrong reaction counts and stale message bodies (GH #22).

We aggregate server-side instead. The store resolves the latest edit per target, groups reactions by target and emoji, and lists replies and thread members — all via expression/partial indexes over `relates_to`, computed at read time (cheap at Riley scale; materialized counters are a later optimization). Raw relation events stay on disk (append-mostly, provenance preserved — the same philosophy as redaction masking); the timeline read _collapses_ edits into their target rather than rewriting rows. Threads are simply the `m.thread` case of this machinery, so they ship in the MVP as part of aggregation rather than as a separate later feature.

### Event provenance

Every event row carries a `provenance` field. For MVP it is always `upstream_homeserver`. The field exists so a future federated ingestion path (a peer Axon importing decrypted history for a shared room) can be modeled cleanly without schema changes. See "Federation deferral" below.

### Event store schema: hybrid hot-columns + JSONB

Hot fields are extracted to indexed columns; full content stays as JSONB. Considered:

- **Pure relational:** every event type gets its own table. Brittle, worst for unknown event types and spec churn.
- **Pure JSONB blob:** simplest schema, awkward indexing and ranking, slow timeline queries.
- **Hybrid (chosen).** Columns: `event_id` (text), `room_id` (text), `account_id` (uuid), `sender` (text), `origin_ts` (bigint — milliseconds since Unix epoch, as Matrix's `origin_server_ts` reports), `type` (text), `redacts` (text, nullable — `event_id` of the redacted event, if this row is a redaction), `relates_to` (jsonb, nullable — Matrix relation block), `decrypted_body_text` (text, nullable — extracted for ranking). Full decrypted content as JSONB. Original ciphertext + megolm session metadata + sender device keys in sibling tables linked by `event_id`. Indexes on the hot fields.

Tradeoff: a bit of write-time cost extracting the hot columns, paid back many times over on timeline pagination, ranking, and filter queries.

### Append-mostly storage and room-lifecycle semantics

The store is append-mostly. Membership changes do not retroactively rewrite history. Leaving / being banned / rooms being deleted upstream do not delete the local archive by default. Per-room retention policy (retain / hide / delete) is exposed but defaults to retain. Room upgrades (`m.room.tombstone`) link old and new rooms in the data model so timeline navigation and search work across the upgrade.

**Redactions and message deletion.** Matrix expresses message deletion as `m.room.redaction` events that point at the redacted event's `event_id`. We follow Matrix semantics:

- Redaction events are stored as ordinary events with `type = m.room.redaction` and `redacts = <target event_id>`.
- The redacted target row stays in place. Its decrypted content is masked at read time when serving timelines (a `redacted_because` field on API responses carries the redaction event ID); the original ciphertext, megolm metadata, and sender keys remain in their sibling tables.
- Clients can opt into seeing the pre-redaction content if they hold the appropriate keys (this is mostly a forensic use case, not the default).
- Hard-deletion of redacted content is on the retention-policy code path: "delete" policy can sweep redacted rows after a grace window. Default policy is retain.
- Search index entries for a redacted event are removed at redaction time so search results don't surface deleted content.

### Content authentication

The store keeps original event bytes (ciphertext for encrypted events, signed JSON for unencrypted), the megolm session ID and re-decryption metadata, and sender device identity and cross-signing chain at the time of receipt. This means decrypted rows can be re-verified against the cryptographic evidence Matrix already provides; we do not invent a separate HMAC or agent-level signing layer.

Verification is exposed as an opt-in API capability: clients fetch decrypted events normally, or request a verification bundle per event / per query when they need it. Most traffic carries no verification overhead. This capability — plus a `sender_trust` field on ordinary timeline reads so clients can flag messages from unverified senders — is delivered in **M7c** (sender-device trust & content authentication); the storage it draws on (sender device identity + cross-signing chain at receipt) lands earlier in the event-store schema.

### Live updates: WebSocket with a custom envelope

One bidirectional WebSocket at `/v1/ws`. Server → client carries timeline events, presence updates, typing notifications, and read-receipt fan-out; client → server carries typing, draft sync, and read markers. Considered:

- **Server-Sent Events:** simpler, browser auto-reconnect, but unidirectional. Client-to-server signals (typing, draft sync, read markers) would need a parallel POST path. Matrix also surfaces frequent client→server signals like presence pings that fit a duplex channel; SSE forces a second pipe for those. Net complexity higher.
- **WebSocket (chosen).** One channel covers both directions. axum has first-class support. Envelope: `{type, account_id, payload}` JSON so every event carries its account.

### Local-API auth: bearer tokens for MVP, OAuth later

MVP issues long-lived bearer tokens via an `axon token issue` CLI subcommand. Each token is bound to a human-readable label / device name, hashed at rest, and individually revocable. axum middleware validates `Authorization: Bearer …` on every `/v1/…` request.

Full OAuth 2.0 + PKCE was on the roadmap and has since shipped ahead of MVP freeze: axon is its own minimal OAuth 2.0 authorization server plus an OIDC relying party to Google/Microsoft, behind the same `TokenVerifier` seam described below (M14, ADR 0054). Sign-in with Apple remains deferred to the iOS client work. The token storage table, the middleware, and the API shape were designed so OAuth issuance could drop in without breaking the wire protocol, and it did. Tradeoffs we accepted for the original bearer-only MVP scope:

- **Win:** alpha ships without owning a security-sensitive authorization-server implementation.
- **Cost:** initial onboarding for `axon-tui` is "paste the token from your CLI" rather than a login flow. Acceptable for the alpha audience.

### API versioning: path-prefix `/v1/`, SemVer on the spec

All routes live under `/v1/…`. SemVer applies to the OpenAPI spec. Breaking changes bump the major and move to `/v2/…`. Previous major remains supported in parallel for a defined window — target: two minor releases past the next major's GA, which gives client authors a real deprecation runway. Considered date-based versioning (Stripe-style) and Accept-header media-type versioning; both are heavier than this API needs.

### Search: backend in MVP, single default analyzer

Tantivy index populated on event ingestion. `account_id` is a facet field; queries can scope to one account or aggregate across all. BM25 ranking. Filters: room, sender, account, date range.

Single language-agnostic analyzer for MVP. The analyzer chain is Tantivy's default tokenizer plus three built-in token filters: `LowerCaser` (case-insensitive matching), `AsciiFoldingFilter` (diacritics fold — `café` matches `cafe`), and `Stemmer` (light morphological stemming — `cats` matches `cat`, `running` matches `run`). These are configuration, not code we write; Tantivy ships them all.

What the analyzer handles vs. what it doesn't, so expectations are explicit:

- **In scope (free, via the chain above):** case-insensitivity, diacritic folding, regular singular/plural and verb-form matching.
- **Typo tolerance:** available but opt-in at query time via Tantivy's `FuzzyTermQuery` (Levenshtein edit distance), not in the analyzer. We can wire a bounded fuzzy mode into the search endpoint later; not required for MVP.
- **Synonyms** (`NYC` ≈ `New York`): not built in; needs a custom filter or query expansion. Out of scope for MVP.
- **Semantic / meaning-based search:** not Tantivy's job (vectors / embeddings). Already deferred as "advanced search (semantic)" on the roadmap.

Two known limitations of the single-analyzer choice, both pointing at the deferred per-language work:

- Stemming is inherently language-specific. A Snowball English stemmer mis-stems French / German / etc. We accept English-ish stemming tuned for Latin-script content.
- The default tokenizer splits on whitespace and punctuation, so it does not segment CJK or other non-space-delimited scripts (those need `lindera` / `tantivy-jieba`-style tokenizers).

Per-language detection, per-room overrides, and CJK tokenization are deferred. Rationale: Riley-shaped users have mostly English / Latin-script content; the cost-benefit of per-language analyzers doesn't pay off at MVP scale, and we keep the door open by versioning the index schema.

**Encryption at rest.** The Tantivy index contains decrypted message text — search is the whole point. It lives on the same disk as Postgres and inherits whatever the operator deploys for filesystem-level encryption (LUKS, dm-crypt, ZFS native encryption, encrypted-volume cloud disks). Application-level encryption of the index would defeat search; we do not attempt it. Per-account content-encryption keys, with whatever search story comes with them (encrypted search schemes, decrypted-on-the-fly query-time indexes, etc.), are a v2 concern and explicitly out of scope for MVP. The threat-model section flags this.

### Bridges

Bridged events flow through as ordinary Matrix events. The agent does not parse mautrix / Beeper / other bridge-specific formats and does not surface a normalized `bridge_metadata` field. Clients render whatever the bridge places in event content. Normalization is on the post-MVP roadmap.

### Onboarding: fresh sync only

First run logs the agent in as a new Matrix device and runs a fresh sliding sync. No importer from Element X / gomuks / Fluffychat / others. The initial-sync success criterion (under ten minutes for a Riley-shape account) is what makes fresh sync acceptable.

### Push deferred entirely

No APNs / FCM / web push code paths in MVP. The event store schema and the event-emit surface inside the agent are designed so a push router can be added later without schema changes. Push is the highest-priority post-MVP track (see Roadmap signposts).

## Open decisions

Almost every architectural question was resolved during planning. The table below maps each resolution. One question remains genuinely open and is called out separately.

| Question                             | Decision                                                                                                                                                                 |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Multi-tenancy model                  | One human per Axon; N Matrix accounts inside, `account_id`-scoped tables. Multi-human-per-process is a non-goal.                                                         |
| Sliding sync vs legacy sync fallback | Simplified Sliding Sync only.                                                                                                                                            |
| Event store schema                   | Hybrid hot-columns + JSONB.                                                                                                                                              |
| Search analyzer defaults             | Single language-agnostic analyzer for MVP.                                                                                                                               |
| Push payload format                  | Push deferred entirely; revisit as a P0 post-MVP track.                                                                                                                  |
| OAuth implementation                 | Bearer tokens for MVP; OAuth 2.0 + PKCE was planned post-MVP but shipped ahead of freeze (M14, ADR 0054).                                                                |
| Live-update transport                | WebSocket with custom envelope.                                                                                                                                          |
| API versioning policy                | Path-prefix `/v1/`, SemVer; previous major supported two minor releases after next major GA.                                                                             |
| Migration story                      | Fresh sync only for MVP.                                                                                                                                                 |
| Bridge event handling                | Treated as ordinary Matrix events; no normalization.                                                                                                                     |
| Media storage backend                | Local disk LRU cache for MVP. S3-compatible storage deferred until a hosted deployment needs it.                                                                         |
| Account lifecycle                    | Explicit account states + active-account gating + runtime login/verify/recover/logout/delete (logout retains the archive; delete purges). In MVP (resolves GH #14, #24). |
| Relation aggregation                 | Server-side, read-time aggregation of edits/reactions/replies/threads over stored relations. In MVP (resolves GH #22).                                                   |
| Threads                              | In MVP, as the `m.thread` case of relation aggregation (no separate milestone).                                                                                          |

**Open: how to handle redacted events?** Should Axon expose and index redacted events or hide them from its user? This depends on how we view the purpose of redactions: are they to get rid of bad content that no one should see, or, e.g., moderation decisions that may want to be reviewed/reverted later?

## Threat model summary

(Flag for what changes when push lands.)

- **Operator trust.** The Axon operator can read all decrypted content for the human it hosts. For Axon this is the human themself, since one Axon hosts one human. Self-hosting is the story.
- **Disk compromise.** Event store and search index live on the same disk and inherit filesystem-level encryption (operator's choice: LUKS / dm-crypt / ZFS / encrypted cloud volumes). Application-level encryption of decrypted content (per-account content-encryption keys) is deferred to v2 — search becomes a research problem at that point.
- **Network — agent ↔ homeserver.** Standard Matrix C-S over TLS.
- **Network — client ↔ agent.** TLS required; bearer tokens scoped per device, revocable individually.
- **Client compromise.** Per-device tokens limit blast radius. Revocation invalidates a single device. Already-pulled history is out of the agent's hands — same as any Matrix client.
- **Compromised agent process.** Worst case: all data for the human owner. Mitigations: process isolation, principle of least privilege, audit logging. Not solved at v1.

**Changes when push lands:** APNs / FCM payload privacy levels become a user-facing setting. The push router becomes another process with access to decrypted content. Threat-model section will need updating in the push design doc.

## Federation deferral

Axon v1 is one-agent-per-human with no agent-to-agent communication. We keep the door open by capturing event provenance now: every event row records where its decrypted content came from (`upstream_homeserver` only, for v1) and preserves the cryptographic evidence (original ciphertext, megolm session, sender device keys, cross-signing chain) so a peer agent's content could be verified against the homeserver's signatures without trusting the peer.

Implications for MVP schema:

- `events.provenance` column exists from day one.
- Original ciphertext and megolm metadata are sibling tables with `event_id` FK, not just optional verification fields.
- Content / metadata separation is observed so a federated path can ingest decrypted content while metadata (read state, drafts) stays per-account.

We do not build any federation code in v1.

## Roadmap signposts

Originally post-MVP, roughly in priority order — several of these have since shipped or begun ahead of MVP freeze, noted inline:

1. **Push** (APNs first, then FCM and web push). P0 immediately after MVP. _(Not started.)_
2. ~~Full OAuth 2.0 + PKCE.~~ _(Shipped ahead of freeze — M14, ADR 0054.)_
3. **Bridge metadata normalization.** _(Not started.)_
4. **Import-from-existing-client onboarding** (Element X store reader, maybe gomuks). _(Not started.)_
5. **Durable media storage** (S3-compatible backend) when a hosted Axon deployment needs it. _(Not started; still explicitly out of scope — see "What not to build" in `implementation.md`.)_
6. **Per-room / per-language search analyzers.** _(Not started.)_
7. **Spaces as first-class API resources.** _(Not started.)_
8. **Native clients** (iOS first, then desktop) and a web client. See [ADR 0031](../adr/0031-client-strategy.md) for the client strategy. _(Web client (`axon-web`) pulled forward and under active parallel development, well beyond the original alpha scope — see `docs/client-parity.md`. iOS/Android/macOS native clients have not started.)_
9. **Federation of agents v2.** _(Not started.)_

(Threads moved _into_ the MVP as part of relation aggregation; they are no longer a post-MVP track.)
