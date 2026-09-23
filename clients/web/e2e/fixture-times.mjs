// `$seed-image:hs`, the sparse-thread test's thread root, is seeded in
// `mock-server.mjs` at `Date.now() - SEED_IMAGE_ROOT_OFFSET_MS`.
//
// The specs deliberately do *not* re-derive that timestamp from this constant.
// Doing so meant a second `Date.now()`, in a second process, at a second
// moment — and when the gap between the two straddled local midnight, a fixture
// meant to sit beside the root landed on the next calendar day and the client
// drew a day separator between them (issue #272, and again after it). A spec
// that needs to sit a fixture next to this root reads the root's real
// `origin_ts` back from the mock instead; see `seedImageRootTs` in
// `layout.spec.ts`. This constant is the mock's own, and has one consumer.
export const SEED_IMAGE_ROOT_OFFSET_MS = 3_600_000
