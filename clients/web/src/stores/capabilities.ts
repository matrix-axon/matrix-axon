import { effect, signal, type ReadonlySignal } from '@preact/signals'
import type { ApiClient } from '../api/client'

/**
 * The optional features this Axon server supports (`GET /v1/capabilities`,
 * ADR 0132 § 8). An older server has no such route, and its `404` means
 * none of them.
 */
export interface ServerCapabilities {
  /**
   * Whether an edit can keep, remove and add link previews: `true`, `false`
   * (a `404`, kept until the next reconnect), or `null` when not yet known,
   * because the answer has not come or the request failed. `null` counts as
   * no.
   */
  editUrlPreviews: ReadonlySignal<boolean | null>
  /** Ask, unless the answer is known or already being asked. */
  refresh(): void
  /**
   * Ask again even though the answer is known, as after a reconnect, which
   * is how a server upgrade shows. The answer stands until a new one comes.
   */
  recheck(): void
  /** Forget the answer, as at sign-out. */
  reset(): void
}

export function createServerCapabilities(api: ApiClient): ServerCapabilities {
  const editUrlPreviews = signal<boolean | null>(null)
  /** Bumped by `reset`, so an answer from before it is dropped. */
  let generation = 0
  let asking = false
  function ask() {
    if (asking) return
    asking = true
    const asked = generation
    void api
      .GET('/v1/capabilities')
      .then(
        ({ data, response }) => {
          if (asked !== generation) return
          if (data !== undefined)
            editUrlPreviews.value = data.data.edit_url_previews === true
          else if (response.status === 404) editUrlPreviews.value = false
          // Any other failure is not an answer: what is known stands, and
          // an edit opened with nothing known asks again.
        },
        () => {},
      )
      .finally(() => {
        if (asked === generation) asking = false
      })
  }
  return {
    editUrlPreviews,
    refresh() {
      if (editUrlPreviews.peek() === null) ask()
    },
    recheck: ask,
    reset() {
      generation += 1
      asking = false
      if (editUrlPreviews.peek() !== null) editUrlPreviews.value = null
    },
  }
}

/**
 * Ask when a session starts, forget the answer when it ends, and ask again
 * whenever the live socket comes back: one server serves every account, and
 * a reconnect is the earliest sign it was restarted, perhaps upgraded (as
 * `connectUpdateChecks` reasons for the web build).
 */
export function connectCapabilities(
  signedIn: ReadonlySignal<boolean>,
  capabilities: ServerCapabilities,
  reconnects?: ReadonlySignal<number>,
): () => void {
  const session = effect(() => {
    if (signedIn.value) capabilities.refresh()
    else capabilities.reset()
  })
  const reconnect = effect(() => {
    if ((reconnects?.value ?? 0) === 0) return
    if (signedIn.peek()) capabilities.recheck()
  })
  return () => {
    session()
    reconnect()
  }
}
