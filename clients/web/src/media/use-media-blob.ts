import type { RefObject } from 'preact'
import { useEffect, useRef, useState } from 'preact/hooks'
import { perfMark } from '../perf'
import { useServices } from '../services'
import { observeVisible } from './intersection'
import type {
  MediaFailure,
  MediaHandle,
  MediaRequestOptions,
  SniffedFormat,
  ThumbnailMethod,
  ThumbnailRequest,
} from './media-service'

export interface MediaBlobState {
  status: 'idle' | 'loading' | 'ready' | 'error'
  url?: string
  /** What the bytes actually are, once they have arrived (`sniff.ts`). */
  format?: SniffedFormat
  error?: MediaFailure
}

/**
 * Lazily resolve an `mxc://` URI to an object URL for one mounted element
 * (ADR 0064). Attach the returned `ref` to the element that reserves the
 * image's space; the fetch starts when that element nears the viewport, and
 * the acquired handle is released on unmount so the blob can be revoked.
 *
 * jsdom has no `IntersectionObserver`, so there the fetch starts eagerly on
 * mount — component tests exercise the real load path without a stub.
 */
/**
 * The `MediaRequestOptions` for one acquire, from the hook's flattened inputs.
 *
 * Shared by `acquire()` and `invalidate()` rather than written out at each,
 * because these options *are* the media cache key: two copies that drifted
 * would leave `invalidate()` addressing a different slot than the one that
 * failed, and the retry would be handed the broken object right back — with
 * nothing to show that anything had gone wrong.
 *
 * Flattened arguments, not a `ThumbnailRequest`, because the hook destructures
 * the request into primitives so its effect dependencies compare by value.
 */
function requestOptions(
  width: number | undefined,
  height: number | undefined,
  method: ThumbnailMethod | undefined,
  contentType: string | undefined,
): MediaRequestOptions | undefined {
  if (width !== undefined && height !== undefined) {
    return { thumbnail: { width, height, method }, contentType }
  }
  return contentType !== undefined ? { contentType } : undefined
}

export function useMediaBlob<T extends HTMLElement = HTMLElement>(
  accountId: string,
  mxcUrl: string | null,
  options: {
    eager?: boolean
    thumbnail?: ThumbnailRequest
    /** See `MediaRequestOptions.contentType` — allowlisted types only. */
    contentType?: string
    /**
     * Bump to re-acquire. Purely an effect trigger — it is deliberately *not*
     * part of the media cache key, since a component-local counter restarts at
     * 0 on remount and would collide with its own earlier generations. Pair it
     * with `invalidate()`, which is what actually guarantees fresh bytes.
     */
    attempt?: number
  } = {},
): {
  ref: RefObject<T>
  state: MediaBlobState
  /**
   * Forget the cached object for this url, so the next acquire refetches.
   *
   * Call it when the bytes themselves proved bad — a decode failure — before
   * bumping `attempt`. Without it the retry is served the same object URL
   * straight from the cache and no request is made at all.
   */
  invalidate: () => void
} {
  const { media } = useServices()
  const ref = useRef<T>(null)
  const [state, setState] = useState<MediaBlobState>({ status: 'idle' })
  const { eager = false, thumbnail, contentType, attempt } = options
  const thumbnailWidth = thumbnail?.width
  const thumbnailHeight = thumbnail?.height
  const thumbnailMethod = thumbnail?.method

  useEffect(() => {
    if (mxcUrl === null) {
      setState({ status: 'idle' })
      return
    }

    let cancelled = false
    let started = false
    let handle: MediaHandle | null = null

    const start = () => {
      if (started) {
        return
      }
      started = true
      perfMark('media-blob:start', {
        accountId,
        mxcUrl,
        thumbnail:
          thumbnailWidth !== undefined && thumbnailHeight !== undefined,
        eager,
      })
      setState({ status: 'loading' })
      void media
        .acquire(
          accountId,
          mxcUrl,
          requestOptions(
            thumbnailWidth,
            thumbnailHeight,
            thumbnailMethod,
            contentType,
          ),
        )
        .then((acquired) => {
          if (cancelled) {
            perfMark('media-blob:cancelled', {
              accountId,
              mxcUrl,
            })
            acquired.release()
            return
          }
          handle = acquired
          perfMark(
            acquired.result.ok ? 'media-blob:ready' : 'media-blob:error',
            {
              accountId,
              mxcUrl,
              thumbnail:
                thumbnailWidth !== undefined && thumbnailHeight !== undefined,
            },
          )
          setState(
            acquired.result.ok
              ? {
                  status: 'ready',
                  url: acquired.result.url,
                  format: acquired.result.format,
                }
              : { status: 'error', error: acquired.result.error },
          )
        })
    }

    let stopObserving: (() => void) | null = null
    if (
      eager ||
      typeof IntersectionObserver === 'undefined' ||
      ref.current === null
    ) {
      start()
    } else {
      stopObserving = observeVisible(ref.current, start)
    }

    return () => {
      cancelled = true
      stopObserving?.()
      handle?.release()
    }
  }, [
    media,
    accountId,
    mxcUrl,
    eager,
    thumbnailWidth,
    thumbnailHeight,
    thumbnailMethod,
    contentType,
    attempt,
  ])

  const invalidate = () => {
    if (mxcUrl === null) {
      return
    }
    media.invalidate(
      accountId,
      mxcUrl,
      requestOptions(
        thumbnailWidth,
        thumbnailHeight,
        thumbnailMethod,
        contentType,
      ),
    )
  }

  return { ref, state, invalidate }
}
