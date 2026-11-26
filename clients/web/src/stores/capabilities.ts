import { batch, effect, signal, type ReadonlySignal } from '@preact/signals'
import type { ApiClient } from '../api/client'
import type { components } from '../api/schema'

/** A kind `POST …/media/uploads` accepts (ADR 0059, ADR 0142 § 1). */
export type MediaUploadKind = components['schemas']['MediaUploadKindDto']

/**
 * What a media send may use beyond ADR 0059's two kinds (ADR 0142 § 1).
 * The part of [`ServerCapabilities`] a timeline store reads, so a test can
 * hand it two signals rather than a capabilities store.
 */
export interface MediaSendSupport {
  /**
   * The kinds staging accepts, or `null` when not yet known. A server that
   * predates the field accepts only `image` and `file`, and so does `null`.
   */
  mediaUploadKinds: ReadonlySignal<readonly MediaUploadKind[] | null>
  /**
   * Whether `send-media` applies `info`, `thumbnail_upload_id` and `txn_id`.
   * `null` (not known) counts as no.
   */
  sendMediaThumbnails: ReadonlySignal<boolean | null>
}

/**
 * The optional features this Axon server supports (`GET /v1/capabilities`,
 * ADR 0132 § 8). An older server has no such route, and its `404` means
 * none of them.
 */
export interface ServerCapabilities extends MediaSendSupport {
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

/** What a server that predates `media_upload_kinds` accepts. */
const LEGACY_UPLOAD_KINDS: readonly MediaUploadKind[] = ['image', 'file']

export function createServerCapabilities(api: ApiClient): ServerCapabilities {
  const editUrlPreviews = signal<boolean | null>(null)
  const mediaUploadKinds = signal<readonly MediaUploadKind[] | null>(null)
  const sendMediaThumbnails = signal<boolean | null>(null)
  /** Bumped by `reset`, so an answer from before it is dropped. */
  let generation = 0
  let asking = false
  function answer(
    edit: boolean,
    kinds: readonly MediaUploadKind[],
    thumbnails: boolean,
  ) {
    batch(() => {
      editUrlPreviews.value = edit
      mediaUploadKinds.value = kinds
      sendMediaThumbnails.value = thumbnails
    })
  }
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
            answer(
              data.data.edit_url_previews === true,
              data.data.media_upload_kinds ?? LEGACY_UPLOAD_KINDS,
              data.data.send_media_thumbnails === true,
            )
          else if (response.status === 404)
            answer(false, LEGACY_UPLOAD_KINDS, false)
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
    mediaUploadKinds,
    sendMediaThumbnails,
    refresh() {
      // All three are set together, so one stands for the answer.
      if (editUrlPreviews.peek() === null) ask()
    },
    recheck: ask,
    reset() {
      generation += 1
      asking = false
      batch(() => {
        if (editUrlPreviews.peek() !== null) editUrlPreviews.value = null
        if (mediaUploadKinds.peek() !== null) mediaUploadKinds.value = null
        if (sendMediaThumbnails.peek() !== null)
          sendMediaThumbnails.value = null
      })
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
