import { render } from 'preact'
import './index.css'
import { App } from './app.tsx'
import { BUILD_INFO } from './build-info.ts'
import {
  browserReloadEnvironment,
  CHUNK_TARGET,
  initReloadGuard,
  reloadOnce,
} from './reload.ts'

const reloadEnv = browserReloadEnvironment()

// Settle any reload we did on the way here, before anything can ask for another.
initReloadGuard(BUILD_INFO.version, reloadEnv)

/**
 * Recover from a chunk that a redeploy deleted (ADR 0087).
 *
 * The app is code-split — every highlight.js language, the PDF viewer, the
 * emoji picker — so a session that outlives a deploy will eventually `import()`
 * a hashed chunk the origin no longer has. Vite raises `vite:preloadError` for
 * exactly this case, and its own guidance is to reload: the failure is not
 * retryable, and the stale thing is the document naming the missing chunk.
 *
 * Registered before `render` so it is in place ahead of the first lazy import.
 * `preventDefault` suppresses Vite's default rethrow — we are handling it.
 * `reloadOnce` is what keeps a chunk that 404s for some *other* reason from
 * turning this into a reload loop; if it declines, the failure surfaces as the
 * feature not loading, which is where we already were.
 */
window.addEventListener('vite:preloadError', (event) => {
  event.preventDefault()
  reloadOnce(BUILD_INFO.version, CHUNK_TARGET, reloadEnv)
})

render(<App />, document.getElementById('app')!)
