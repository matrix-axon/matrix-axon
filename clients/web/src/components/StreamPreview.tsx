import type { ComponentChildren } from 'preact'
import { useCallback, useEffect, useRef, useState } from 'preact/hooks'
import type { ParsedMedia } from '../media/parse-media'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { MediaPlayer } from './MediaPlayer'

/** `MediaError.MEDIA_ERR_NETWORK`, spelled out for engines without the global. */
const MEDIA_ERR_NETWORK = 2
/** `HTMLMediaElement.HAVE_FUTURE_DATA`: enough to play on from here. */
const HAVE_FUTURE_DATA = 3

/**
 * When a stall is probed. `stalled` already means the engine got no data for
 * about three seconds, so it is probed a second later unless data (a
 * `progress` event) has arrived meanwhile. A bare `waiting` gets longer,
 * since a slow link raises it too, and is probed only if playback has not
 * moved. Stall probes are at most one per `minInterval`, so a slow link that
 * keeps raising `stalled` costs a one-byte request every ten seconds at most.
 */
export const STALL_PROBE_MS = {
  stalled: 1_000,
  waiting: 5_000,
  minInterval: 10_000,
}

type State =
  | { status: 'issuing' }
  | { status: 'ready'; src: string }
  /** The server will not stream this object; the blob path takes over. */
  | { status: 'fallback' }
  /** The bytes arrive, but this browser cannot decode them. */
  | { status: 'unplayable' }
  /** The encrypted attachment failed its hash check. Terminal. */
  | { status: 'undecryptable' }
  /** Anything else. Offers Retry. */
  | { status: 'failed' }

/**
 * Play audio or video from a stream-ticket URL (ADR 0139 § 6), so the browser
 * streams it with its own range requests and starts playing on the first few
 * hundred KiB, rather than the page buffering the whole object into a blob.
 *
 * Mounted only once the user opens the item, like every preview body: a
 * ticket is issued, and a download started, only for media someone chose to
 * play. The poster stays on the blob path, being a small image.
 *
 * A media element reports a refused request and an unsupported codec alike,
 * so on `error` the ticket URL is probed with a one-byte range to tell them
 * apart. An expired ticket (the server restarted, or its OAuth access token
 * ran out) is replaced once and playback resumes where it stopped; the
 * allowance comes back once the new ticket has loaded, so an hour-long video
 * across several token lifetimes keeps recovering.
 *
 * WebKit's AVFoundation player never raises `error` for a refused range once
 * playback has begun: it re-requests the range, tens of times a second, for as
 * long as the page is open. It keeps playing from its buffer meanwhile, and
 * even reports `networkState` idle, so neither a frozen playhead nor the
 * network state gives it away. What it does raise is `stalled`, a few seconds
 * after the data stops. So a `stalled` that no `progress` follows is probed,
 * as is a `waiting` that playback does not move on from. A reachable probe
 * means a slow link, and the player is left to wait; any other answer is
 * handled as it is for `error`, and replacing or removing the `src` ends the
 * request loop.
 */
export function StreamPreview({
  accountId,
  media,
  kind,
  fallback,
}: {
  accountId: string
  media: ParsedMedia
  kind: 'audio' | 'video'
  /** What to render when the server refuses to stream the object. */
  fallback: () => ComponentChildren
}) {
  const { streamTickets } = useServices()
  const [state, setState] = useState<State>({ status: 'issuing' })
  // Bumped by Retry, to issue a fresh ticket from scratch.
  const [attempt, setAttempt] = useState(0)
  const mxcUrl = media.url

  // The last element rendered, kept past Preact nulling the ref on unmount so
  // the cleanup below can still stop its download.
  const element = useRef<HTMLMediaElement | null>(null)
  // Where to resume, and whether to keep playing, once a replacement ticket's
  // metadata arrives.
  const resume = useRef<{ at: number; play: boolean } | null>(null)
  const playing = useRef(false)
  // One ticket replacement per successful load (see the doc comment).
  const mayReplace = useRef(true)
  // Bumped whenever the ticket is replaced or abandoned, so a slow probe or
  // issue cannot land on a newer one.
  const generation = useRef(0)
  // At most one probe at a time, from either trigger.
  const probing = useRef(false)
  const stallTimers = useRef(new Map<'stalled' | 'waiting', number>())
  // Counts `progress` events: a `stalled` that one follows was a hiccup.
  const progressed = useRef(0)
  const lastStallProbe = useRef(-Infinity)
  // The state as of the last `settle`, for callbacks armed by an earlier
  // render: a timer or a probe continuation must not act on a closed-over
  // `ready` the player has since left.
  const current = useRef<State>(state)

  // Both touch only refs and the state setter, so they are stable for the
  // component's life and safe in the effects below.
  const clearStallTimers = useCallback(() => {
    for (const timer of stallTimers.current.values()) clearTimeout(timer)
    stallTimers.current.clear()
  }, [])

  /**
   * Move to `next`. A state with no player in it is final until Retry, so it
   * also retires everything armed for the old player: pending stall timers
   * are cleared, and `generation` moves on so an in-flight probe or issue
   * cannot revive or overwrite what the user was shown.
   */
  const settle = useCallback(
    (next: State) => {
      current.current = next
      if (next.status !== 'ready' && next.status !== 'issuing') {
        generation.current += 1
        clearStallTimers()
      }
      setState(next)
    },
    [clearStallTimers],
  )

  useEffect(() => {
    if (mxcUrl === null) {
      settle({ status: 'failed' })
      return
    }
    const mine = ++generation.current
    mayReplace.current = true
    settle({ status: 'issuing' })
    void streamTickets.issue(accountId, mxcUrl).then((outcome) => {
      if (mine !== generation.current) return
      if (outcome.ok) {
        settle({ status: 'ready', src: outcome.src })
      } else {
        settle({ status: outcome.kind === 'refused' ? 'fallback' : 'failed' })
      }
    })
  }, [streamTickets, accountId, mxcUrl, attempt, settle])

  // Closing the preview must also stop the browser's download: a detached
  // element with a `src` can keep fetching until it is collected.
  useEffect(
    () => () => {
      generation.current += 1
      clearStallTimers()
      const el = element.current
      if (el !== null) {
        el.pause()
        el.removeAttribute('src')
        el.load()
      }
    },
    [clearStallTimers],
  )

  const poster = useMediaBlob<HTMLDivElement>(
    accountId,
    kind === 'video' ? media.thumbnailUrl : null,
    { eager: true },
  )

  /** Probe the ticket after an `error` or a stall, and act on the answer. */
  const diagnose = async (trigger: 'error' | 'stall') => {
    const playingFrom = current.current
    if (playingFrom.status !== 'ready' || mxcUrl === null || probing.current) {
      return
    }
    if (trigger === 'stall') {
      if (Date.now() - lastStallProbe.current < STALL_PROBE_MS.minInterval) {
        return
      }
      lastStallProbe.current = Date.now()
    }
    const el = element.current
    const code = el?.error?.code
    const at =
      el === null ? null : { at: el.currentTime, play: playing.current }
    const mine = generation.current
    probing.current = true
    const probe = await streamTickets.probe(playingFrom.src).finally(() => {
      probing.current = false
    })
    if (mine !== generation.current) return
    if (trigger === 'stall' && probe === 'reachable') {
      // Slow, not broken: the player keeps its own retry, and its next
      // `stalled` or `waiting` is watched as this one was.
      return
    }
    // Kept for whatever comes next: both a replacement ticket and a Retry
    // pick up from here.
    resume.current = at
    switch (probe) {
      case 'reachable':
        // A network error with reachable bytes was a dropped connection, not
        // a codec: offer Retry rather than blaming the browser.
        settle({
          status: code === MEDIA_ERR_NETWORK ? 'failed' : 'unplayable',
        })
        return
      case 'undecryptable':
        settle({ status: 'undecryptable' })
        return
      case 'failed':
        settle({ status: 'failed' })
        return
      case 'expired':
        break
    }
    if (!mayReplace.current) {
      settle({ status: 'failed' })
      return
    }
    mayReplace.current = false
    const replacing = ++generation.current
    const outcome = await streamTickets.issue(accountId, mxcUrl)
    if (replacing !== generation.current) return
    settle(
      outcome.ok
        ? { status: 'ready', src: outcome.src }
        : {
            status: outcome.kind === 'refused' ? 'fallback' : 'failed',
          },
    )
  }

  /**
   * Probe after a `stalled` that no data follows, or a `waiting` that
   * playback does not move on from. A repeat of the same event restarts its
   * wait, so a burst of them costs one probe.
   */
  const watchStall = (kind: 'stalled' | 'waiting') => {
    const el = element.current
    if (el === null) return
    const timers = stallTimers.current
    const pending = timers.get(kind)
    if (pending !== undefined) clearTimeout(pending)
    const from = el.currentTime
    const progressedFrom = progressed.current
    const mine = generation.current
    const timer = window.setTimeout(() => {
      timers.delete(kind)
      const now = element.current
      if (mine !== generation.current || now === null) return
      if (kind === 'stalled') {
        if (progressed.current === progressedFrom) void diagnose('stall')
        return
      }
      if (now.currentTime !== from) return
      // Stuck means a seek or playback that is not moving, or no data to
      // move with. `readyState` alone is not enough: AVFoundation reports
      // HAVE_ENOUGH_DATA throughout a seek its range requests are refused for.
      const stuck =
        now.seeking || !now.paused || now.readyState < HAVE_FUTURE_DATA
      if (stuck) void diagnose('stall')
    }, STALL_PROBE_MS[kind])
    timers.set(kind, timer)
  }

  const onLoadedMetadata = () => {
    const el = element.current
    const at = resume.current
    resume.current = null
    if (el === null || at === null) return
    if (at.at > 0) {
      el.currentTime = at.at
    }
    if (at.play) {
      // A rejected play() (an autoplay policy) leaves the controls to the
      // user, which is where they were anyway.
      void el.play()?.catch(() => {})
    }
  }

  const label = kind === 'audio' ? 'audio' : 'video'
  let body
  switch (state.status) {
    case 'fallback':
      return <>{fallback()}</>
    case 'issuing':
      body = <div class="media-skeleton" aria-hidden="true" />
      break
    case 'unplayable':
      body = (
        <p class="local-echo-status error">
          This browser cannot play {media.filename} — use Download to open it in
          another app.
        </p>
      )
      break
    case 'undecryptable':
      body = (
        <p class="local-echo-status error">Could not decrypt this {label}.</p>
      )
      break
    case 'failed':
      body = (
        <p class="local-echo-status error">
          Could not load {label}
          <button
            type="button"
            class="ghost"
            onClick={() => setAttempt((n) => n + 1)}
          >
            Retry
          </button>
        </p>
      )
      break
    case 'ready':
      body = (
        <MediaPlayer
          kind={kind}
          src={state.src}
          poster={
            poster.state.status === 'ready' ? poster.state.url : undefined
          }
          width={media.w}
          height={media.h}
          elementRef={(el: HTMLMediaElement | null) => {
            if (el !== null) element.current = el
          }}
          onError={() => void diagnose('error')}
          onStalled={() => watchStall('stalled')}
          onWaiting={() => watchStall('waiting')}
          onProgress={() => {
            progressed.current += 1
          }}
          onLoadedMetadata={onLoadedMetadata}
          onLoadedData={() => {
            mayReplace.current = true
          }}
          onPlay={() => {
            playing.current = true
          }}
          onPause={() => {
            playing.current = false
          }}
        />
      )
      break
  }
  return <div class="lightbox-media">{body}</div>
}
