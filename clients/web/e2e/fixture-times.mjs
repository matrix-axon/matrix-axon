// Shared between `mock-server.mjs` (a plain Node process) and the Playwright
// specs (transpiled TS) — plain `.mjs` so both can import it directly with no
// build step.
//
// `$seed-image:hs`, the sparse-thread test's thread root, is seeded in
// `mock-server.mjs` at `Date.now() - SEED_IMAGE_ROOT_OFFSET_MS`. Its reply in
// `layout.spec.ts` is anchored to that same offset rather than a bare
// `Date.now()`, which could land the two on different UTC calendar days and
// insert an unexpected day separator between root and reply (issue #272).
// Keeping the offset in one place means a future change to it can't
// silently reopen that flake with no compiler or test signal.
export const SEED_IMAGE_ROOT_OFFSET_MS = 3_600_000
