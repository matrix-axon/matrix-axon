import { useState } from 'preact/hooks'
import { browserPlatform, type Platform } from './platform'
import {
  httpFallbackFor,
  normalizeServerUrl,
  storeServerUrl,
} from './server-url'

/**
 * How long to wait for `/healthz` before giving up. Every outbound call gets a
 * timeout (repo-wide boundary rule); here the failure mode is a typo'd host
 * that never answers, and a setup screen that hangs with no way back is worse
 * than one that says "could not reach it".
 */
const PROBE_TIMEOUT_MS = 8000

type Status =
  | { state: 'idle' }
  | { state: 'probing' }
  | { state: 'failed'; message: string }

/**
 * First-run "which Axon server?" screen (ADR 0102 § 3).
 *
 * Only ever reached on a platform with no same-origin API to fall back on —
 * i.e. a packaged build. A browser deployment resolves `'/'` and never renders
 * this.
 *
 * The entry is probed against `GET /healthz` before it is stored. That route
 * is deliberately the right one: it lives *outside* `/v1`, so it needs no
 * token and can be checked before the user has any credential, and
 * `deploy/web/Caddyfile` already proxies it for exactly this kind of check.
 * Storing an unreachable URL would drop the user into a sign-in screen that
 * cannot work, with nothing pointing at the real mistake.
 */
export function ServerSetup({
  onConnected,
  platform = browserPlatform(),
  storage = window.localStorage,
}: {
  onConnected: (baseUrl: string) => void
  platform?: Pick<Platform, 'fetch'>
  storage?: Storage
}) {
  const [draft, setDraft] = useState('')
  const [status, setStatus] = useState<Status>({ state: 'idle' })

  const normalized = normalizeServerUrl(draft)
  const probing = status.state === 'probing'

  async function connect(): Promise<void> {
    if (normalized === null || probing) {
      return
    }
    setStatus({ state: 'probing' })

    const abort = new AbortController()
    const timer = setTimeout(() => abort.abort(), PROBE_TIMEOUT_MS)
    try {
      // https first, then plain http for a local address the user typed
      // without a scheme — see `httpFallbackFor`. An explicit scheme is never
      // second-guessed.
      const candidates = [normalized, httpFallbackFor(draft, normalized)]
      let lastStatus: number | null = null
      for (const candidate of candidates) {
        if (candidate === null) {
          continue
        }
        let res: Response
        try {
          res = await platform.fetch(`${candidate}/healthz`, {
            signal: abort.signal,
          })
        } catch {
          continue
        }
        if (res.ok) {
          storeServerUrl(storage, candidate)
          onConnected(candidate)
          return
        }
        lastStatus = res.status
      }
      if (lastStatus !== null) {
        setStatus({
          state: 'failed',
          message: `That address answered with ${lastStatus}. Is it an Axon server?`,
        })
        return
      }
      throw new Error('unreachable')
    } catch {
      // One message for abort, DNS failure, refused connection and TLS error
      // alike: the browser does not tell us which, and the user's next move is
      // the same in every case.
      setStatus({
        state: 'failed',
        message: `Could not reach ${normalized}. Check the address and that the server is running.`,
      })
    } finally {
      clearTimeout(timer)
    }
  }

  return (
    <main class="signin">
      <h1>axon</h1>
      <p>Connect to your Axon server to get started.</p>
      <form
        class="server-setup"
        onSubmit={(event) => {
          event.preventDefault()
          void connect()
        }}
      >
        <label>
          Server address
          {/*
           * `type="text"`, deliberately, with only the keyboard hint from
           * `inputMode`. `type="url"` looks like the right control and is not:
           * it applies native constraint validation, under which a bare
           * `axon.example.com` is a typeMismatch, so the browser silently
           * blocks submit — killing the "https:// is assumed if you leave it
           * out" affordance this screen advertises. Validation here is
           * `normalizeServerUrl`, which is more forgiving on the way in and
           * stricter about the scheme on the way out.
           */}
          <input
            type="text"
            inputMode="url"
            autocomplete="url"
            autocapitalize="none"
            autocorrect="off"
            spellcheck={false}
            value={draft}
            placeholder="axon.example.com"
            disabled={probing}
            onInput={(event) => {
              setDraft(event.currentTarget.value)
              setStatus({ state: 'idle' })
            }}
          />
        </label>
        <p class="server-setup-hint">
          {normalized === null
            ? 'A hostname or full URL. https:// is assumed if you leave it out.'
            : `Will connect to ${normalized}`}
        </p>
        <button type="submit" disabled={normalized === null || probing}>
          {probing ? 'Connecting…' : 'Connect'}
        </button>
        {status.state === 'failed' && (
          <p class="server-setup-error" role="alert">
            {status.message}
          </p>
        )}
      </form>
    </main>
  )
}
