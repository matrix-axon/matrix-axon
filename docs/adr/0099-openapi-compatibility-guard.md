# ADR 0099 — API compatibility policy and an automated breaking-change guard

## In brief

Adopt an additive-first policy for `openapi/openapi.json`: don't remove or
retype something a client may already depend on without a deprecation window.
Enforce it with a new CI check, `openapi-compat`, that runs `oasdiff breaking`
between a PR's base and head spec and fails the PR on a detected breaking
change — mirroring `migrations-immutable`'s shape: a standalone
`pull_request`-triggered workflow backed by a script anyone can run locally,
with a commit-trailer escape hatch for a deliberate exception.

## Context

`clients/web`'s `check:api` CI step (`pnpm check:api`) only verifies that
`clients/web/src/api/schema.d.ts` was regenerated from `openapi/openapi.json`
— a drift check between a generated artifact and its source. It has nothing to
say about whether the *content* of that change is compatible with a client
that hasn't upgraded yet: a PR that renames a field, removes an endpoint, or
changes a response shape passes `check:api` cleanly as long as `schema.d.ts`
was regenerated to match.

This gap surfaced from a real tension in AGENTS.md's silo rule: every API
change forced an exception to "PRs don't cross silos" because `check:api`
requires touching both `openapi/` (`crates/`) and `schema.d.ts`
(`clients/web`) in the same PR. Narrowing the silo rule to behavioral changes
and carving out a mechanical exception for generated sync artifacts (this PR)
resolves that tension, but it also removed the only place "did this API
change break someone" was even being asked, however incidentally.

`axon-web` and the server are not actually at risk here: they're served
through one front door and redeploy together (ADR 0087), so the compatibility
window is a few seconds around a restart, already handled by that ADR's
reconnect-triggered reload. **`axon-tui`** (and eventually third-party or iOS
clients, per `docs/client-parity.md`) is the real exposure: it's an
independently distributed binary that can run against an upgraded server for
an arbitrary length of time. There's no version negotiation anywhere in the
project — `openapi.json`'s version has sat static since the beginning — so
today the only thing standing between a server change and a broken old client
is a human noticing during review.

## Decision

**Policy:** evolve `/v1/` additively. Don't remove a path or operation, make
an optional field required, remove a field a client may read, narrow a type,
or remove an enum value, without an explicit deprecation window. This is a
restatement of existing intent (`docs/client-parity.md` already tracks
capability adoption per client on this assumption) rather than a new
constraint.

**Enforcement:** `scripts/check-openapi-compat.sh [<base-ref>]`, run from a
new `openapi-compat` pull-request workflow (`.github/workflows/`) and from a
new pre-push hook in `.pre-commit-config.yaml`, both patterned directly on
`check-migrations-immutable.sh` / `migrations-immutable.yml`:

- Skips entirely if `openapi/openapi.json` is unchanged since the base ref —
  no network call on the common case.
- Otherwise diffs the base and head copies of the spec with
  [`oasdiff breaking --fail-on ERR`](https://github.com/oasdiff/oasdiff),
  downloaded as a pinned, checksum-verified release binary (no Go toolchain
  in this repo to `go install` it with). Verified locally against this repo's
  own history (PR #327's real `openapi.json` diff — 3.9k insertions, 1.2k
  deletions — reports no breaking changes; a synthetic removed path is
  correctly flagged and fails the check with exit 1).
- A deliberate breaking change declares itself with an
  `API-Breaking-Change: <reason>` trailer on a commit in the PR, checked
  across the whole `base..HEAD` range — the same shape as
  `Migration-edit-approved`, justified in the PR description like that
  trailer already is.

**Why `oasdiff` over a hand-rolled diff:** correctly classifying a schema
change as breaking or not (required-field additions, enum narrowing,
parameter changes, response-vs-request asymmetry) is exactly the kind of
domain logic this codebase's own guardrails warn against re-deriving ad hoc.
`oasdiff` is the established tool for this and ships a `breaking` subcommand
with the exact semantics needed (`--fail-on ERR`), rather than a bespoke
script re-implementing OpenAPI semantics badly.

**Why a downloaded binary instead of an npm/cargo dependency:** no
JavaScript or Rust package provides equivalent, actively-maintained
breaking-change detection at the time of writing; the closest npm wrapper is
an unofficial single-maintainer repackaging of the same upstream binary. A
pinned version with a checksum verified against the upstream release, fetched
from GitHub (which every CI run here already depends on), is a smaller trust
surface than an extra unofficial npm dependency wrapping the same binary.

## Consequences

- A PR that changes `openapi/openapi.json` in an incompatible way fails CI
  immediately, with the specific `oasdiff` finding printed, instead of
  surfacing later as a field a shipped TUI silently stops reading.
- Widening the OpenAPI spec (new endpoints, new optional fields, new enum
  variants) continues to pass with no extra ceremony.
- A genuinely necessary breaking change is still possible — it just has to be
  named as one (`API-Breaking-Change: <reason>`) rather than sliding through
  unremarked, and the reason travels with the commit.
- This is a detection guard, not the version-negotiation protocol Adam's
  original comment gestured at ("if third parties are developing clients,
  that's more important"). No runtime code changes: the server still doesn't
  know what version of the contract a connecting TUI expects, and an operator
  running a mismatched server/TUI pair today only gets a merge-time signal
  that the change was breaking, not a runtime rejection. That protocol is a
  separate, materially larger decision, warranted only once a client the
  project doesn't control both ends of actually exists.
