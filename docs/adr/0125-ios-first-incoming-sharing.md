# ADR 0125 — iOS-first incoming sharing with rich links

Status: proposed

## Context

The packaged Axon clients cannot receive text, links or media from the system share sheet.
iOS serves most current mobile users, so its native integration comes first.
Both mobile apps share a Tauri/Preact client, but iOS Share Extensions and Android sharing intents have different entry points and lifecycles.
PR [740](https://github.com/matrix-axon/matrix-axon/pull/740) added text-only bundled link-card rendering; rich-link sending, preview generation and preview images need additional support.

## Proposed decision

Use the architecture, PR sequence and acceptance criteria in the [incoming mobile sharing design](../mobile-incoming-sharing.md).
Track delivery in issue [564](https://github.com/matrix-axon/matrix-axon/issues/564), with one sub-issue per planned PR.

Implement shared server prerequisites for bounded bundled preview metadata, protected thumbnail sending and opt-in homeserver preview generation before dependent client behavior.
Extend the shared card renderer to use authenticated Matrix media, including encrypted preview images, without fetching linked pages or arbitrary remote images during rendering.
Preserve source-supplied metadata when available and retain a plain-link fallback when generation is unavailable.

Implement iOS first with a native Share Extension that selects the account/room and reviews/sends content independently of the main app.
Use coordinated App Group staging and shared Keychain access, with the main app retaining refresh-token ownership.
Support text/rich links before enabling single and multiple media/file sharing, including rich-link captions.
Implement Android afterward through native sharing-intent ingestion and the shared Preact picker/composer.

Use a versioned incoming-share contract, bounded durable staging and explicit ownership/acknowledgment.
Persist per-item delivery results, preserve partial batches, and require explicit resolution of uncertain sends because current send APIs lack caller idempotency keys.
Validate signing, actual provider inputs, lifecycle and resource budgets on signed devices; web tests alone cannot establish native acceptance.

## Consequences

Common metadata, delivery and rendering semantics span both operating systems while native registration and presentation remain platform-specific.
The complete feature spans several server and client PRs, each confined to its silo and carrying its own tests and documentation.
Preview generation depends on homeserver support and explicit user opt-in; a rich card cannot be guaranteed for every shared URL.
Interrupted work remains recoverable without silently resending confirmed or uncertain items.
The detailed design records proposed limits and device exit gates; accepting this ADR does not claim implementation or native validation.
