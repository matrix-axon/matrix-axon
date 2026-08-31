import { cleanup } from '@testing-library/preact'
import { afterEach } from 'vitest'

/**
 * Unmount anything a test rendered, after every test in every file.
 *
 * testing-library registers this itself, but only when it can see a global
 * `afterEach` — i.e. under `globals: true`, which this project deliberately
 * does not enable: that needs `"types": ["vitest/globals"]`, and
 * `tsconfig.app.json` includes `src`, so `describe`/`it`/`expect` would leak
 * into the application's own type namespace.
 *
 * Registering it here gets the same safety explicitly. Without it, cleanup is
 * a per-file obligation that nothing enforces, and a file that forgets does
 * not fail on its own: renders pile up in one DOM and the damage surfaces
 * later as `Found multiple elements with the text of: …` in some other test,
 * pointing nowhere near the file that caused it.
 *
 * Files that already register their own teardown keep it. `cleanup()` is
 * idempotent, so the overlap is harmless, and the msw suites need their
 * `afterEach` for `server.resetHandlers()` regardless.
 */
afterEach(cleanup)

/**
 * Global test-environment shims. jsdom here implements neither `matchMedia`
 * nor a working `localStorage` (the latter is injected per-store instead, see
 * `memory-storage.ts`).
 *
 * `matches: false` makes every media query report "not matching", which is the
 * wide two-pane layout (ADR 0062) — the right default for component tests.
 * A test that needs the narrow branch stubs `window.matchMedia` itself.
 */
if (typeof window.matchMedia !== 'function') {
  window.matchMedia = (query: string): MediaQueryList =>
    ({
      media: query,
      matches: false,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }) as MediaQueryList
}

/**
 * jsdom implements neither `URL.createObjectURL` nor `revokeObjectURL`, which
 * the media layer (ADR 0064) depends on. A counter-backed stub is enough for
 * component tests; the media-service test replaces these with spies to assert
 * the revocation bookkeeping. Deliberately no `IntersectionObserver` stub — its
 * absence drives components down the eager-load fallback the timeline uses.
 */
if (typeof URL.createObjectURL !== 'function') {
  let counter = 0
  URL.createObjectURL = () => `blob:mock/${counter++}`
  URL.revokeObjectURL = () => {}
}
