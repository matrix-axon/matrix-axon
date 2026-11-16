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
   * (a `404`, kept for the session), or `null` when not yet known, because
   * the answer has not come or the request failed. `null` counts as no.
   */
  editUrlPreviews: ReadonlySignal<boolean | null>
  /** Ask, unless the answer is known or already being asked. */
  refresh(): void
  /** Forget the answer, as at sign-out. */
  reset(): void
}

export function createServerCapabilities(api: ApiClient): ServerCapabilities {
  const editUrlPreviews = signal<boolean | null>(null)
  /** Bumped by `reset`, so an answer from before it is dropped. */
  let generation = 0
  let asking = false
  return {
    editUrlPreviews,
    refresh() {
      if (editUrlPreviews.peek() !== null || asking) return
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
            // Any other failure is not an answer: the next edit asks again.
          },
          () => {},
        )
        .finally(() => {
          if (asked === generation) asking = false
        })
    },
    reset() {
      generation += 1
      asking = false
      if (editUrlPreviews.peek() !== null) editUrlPreviews.value = null
    },
  }
}

/**
 * Ask when a session starts, and forget the answer when it ends. A web
 * client's server does not change within a session, so one answer serves
 * every account.
 */
export function connectCapabilities(
  signedIn: ReadonlySignal<boolean>,
  capabilities: ServerCapabilities,
): () => void {
  return effect(() => {
    if (signedIn.value) capabilities.refresh()
    else capabilities.reset()
  })
}
