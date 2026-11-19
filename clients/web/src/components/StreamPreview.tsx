import type { ComponentChildren } from 'preact'
import { useEffect, useRef, useState } from 'preact/hooks'
import type { ParsedMedia } from '../media/parse-media'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { MediaPlayer } from './MediaPlayer'

/** `MediaError.MEDIA_ERR_NETWORK`, spelled out for engines without the global. */
const MEDIA_ERR_NETWORK = 2
/** `HTMLMediaElement.HAVE_FUTURE_DATA`: enough to play on from here. */
const HAVE_FUTURE_DATA = 3

/**
 * How long a player may sit without progress before the ticket is probed.
 * `stalled` already means the engine got no data for about three seconds, so
 * it is checked almost at once; a bare `waiting` gets longer, since a slow
 * link raises it too. After a stall whose probe found the bytes reachable,
 * the check repeats at the slower pace for as long as the player stays stuck.
 */
export const STALL_PROBE_MS = {
  stalled: 1_000,
  waiting: 5_000,
  recheck: 15_000,
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
 * playback has begun: it raises `stalled` and re-requests the range, about
 * ninety times a second, for as long as the page is open. So a player that
 * stalls without progress is probed too. A reachable probe there means a slow
 * link, and the player is left to wait; any other answer is handled as it is
 * for `error`, and replacing or removing the `src` ends the request loop.
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
  const stallTimer = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    if (mxcUrl === null) {
      setState({ status: 'failed' })
      return
    }
    const mine = ++generation.current
    mayReplace.current = true
    setState({ status: 'issuing' })
    void streamTickets.issue(accountId, mxcUrl).then((outcome) => {
      if (mine !== generation.current) return
      if (outcome.ok) {
        setState({ status: 'ready', src: outcome.src })
      } else {
        setState({ status: outcome.kind === 'refused' ? 'fallback' : 'failed' })
      }
    })
  }, [streamTickets, accountId, mxcUrl, attempt])

  // Closing the preview must also stop the browser's download: a detached
  // element with a `src` can keep fetching until it is collected.
  useEffect(
    () => () => {
      generation.current += 1
      if (stallTimer.current !== null) clearTimeout(stallTimer.current)
      const el = element.current
      if (el !== null) {
        el.pause()
        el.removeAttribute('src')
        el.load()
      }
    },
    [],
  )

  const poster = useMediaBlob<HTMLDivElement>(
    accountId,
    kind === 'video' ? media.thumbnailUrl : null,
    { eager: true },
  )

  /** Probe the ticket after an `error` or a stall, and act on the answer. */
  const diagnose = async (trigger: 'error' | 'stall') => {
    if (state.status !== 'ready' || mxcUrl === null || probing.current) return
    const el = element.current
    const code = el?.error?.code
    const at =
      el === null ? null : { at: el.currentTime, play: playing.current }
    const mine = generation.current
    probing.current = true
    const probe = await streamTickets.probe(state.src).finally(() => {
      probing.current = false
    })
    if (mine !== generation.current) return
    if (trigger === 'stall' && probe === 'reachable') {
      // Slow, not broken: the player keeps its own retry, and this keeps
      // watching.
      watchStall(STALL_PROBE_MS.recheck)
      return
    }
    // Kept for whatever comes next: both a replacement ticket and a Retry
    // pick up from here.
    resume.current = at
    switch (probe) {
      case 'reachable':
        // A network error with reachable bytes was a dropped connection, not
        // a codec: offer Retry rather than blaming the browser.
        setState({
          status: code === MEDIA_ERR_NETWORK ? 'failed' : 'unplayable',
        })
        return
      case 'undecryptable':
        setState({ status: 'undecryptable' })
        return
      case 'failed':
        setState({ status: 'failed' })
        return
      case 'expired':
        break
    }
    if (!mayReplace.current) {
      setState({ status: 'failed' })
      return
    }
    mayReplace.current = false
    const replacing = ++generation.current
    const outcome = await streamTickets.issue(accountId, mxcUrl)
    if (replacing !== generation.current) return
    setState(
      outcome.ok
        ? { status: 'ready', src: outcome.src }
        : {
            status: outcome.kind === 'refused' ? 'fallback' : 'failed',
          },
    )
  }

  /**
   * Probe after `delay` if the player has made no progress by then. Each new
   * stall event restarts the wait, so a burst of them costs one probe.
   */
  const watchStall = (delay: number) => {
    const el = element.current
    if (el === null) return
    if (stallTimer.current !== null) clearTimeout(stallTimer.current)
    const from = el.currentTime
    const mine = generation.current
    stallTimer.current = setTimeout(() => {
      stallTimer.current = null
      const now = element.current
      if (mine !== generation.current || now === null) return
      if (now.currentTime !== from) return
      // Stuck means a seek or playback that is not moving, or no data to
      // move with. `readyState` alone is not enough: AVFoundation reports
      // HAVE_ENOUGH_DATA throughout a seek its range requests are refused for.
      const stuck =
        now.seeking || !now.paused || now.readyState < HAVE_FUTURE_DATA
      if (stuck) void diagnose('stall')
    }, delay)
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
          elementRef={(el: HTMLMediaElement | null) => {
            if (el !== null) element.current = el
          }}
          onError={() => void diagnose('error')}
          onStalled={() => watchStall(STALL_PROBE_MS.stalled)}
          onWaiting={() => watchStall(STALL_PROBE_MS.waiting)}
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
