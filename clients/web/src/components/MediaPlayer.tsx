import type { JSX, Ref } from 'preact'

/**
 * The sender's stated video size as the CSS variables `.lightbox-player.sized`
 * reads, or `null` when it is missing or not a usable size.
 *
 * Until its metadata arrives a `<video>` is as large as its poster, the sender's
 * thumbnail, and then grows to the video's own size. A streamed video shows
 * that poster for a moment first, so without this the player visibly jumps.
 */
export function videoSizeStyle(
  w: number | undefined,
  h: number | undefined,
): JSX.CSSProperties | null {
  const usable = (n: number | undefined): n is number =>
    n !== undefined && Number.isFinite(n) && n >= 1 && n <= 16384
  if (!usable(w) || !usable(h)) return null
  return {
    '--video-w': String(Math.round(w)),
    '--video-h': String(Math.round(h)),
  }
}

/**
 * The native `<audio>` or `<video>` element of an attachment preview (ADR
 * 0072), shared by the blob path and the stream-ticket path (ADR 0139 § 6),
 * which differ only in where `src` comes from and what an error means.
 */
export function MediaPlayer({
  kind,
  src,
  poster,
  elementRef,
  onError,
  onLoadedMetadata,
  onLoadedData,
  onPlay,
  onPause,
  onStalled,
  onWaiting,
  onProgress,
  width,
  height,
}: {
  kind: 'audio' | 'video'
  src: string
  /** Video only: the sender's thumbnail, as an object URL. */
  poster?: string
  /** Video only: the sender's `info.w` and `info.h`, to size the player
   * before its metadata arrives. */
  width?: number
  height?: number
  elementRef?: Ref<HTMLMediaElement>
  onError: () => void
  onLoadedMetadata?: () => void
  onLoadedData?: () => void
  onPlay?: () => void
  onPause?: () => void
  onStalled?: () => void
  onWaiting?: () => void
  onProgress?: () => void
}) {
  const handlers = {
    onError,
    onLoadedMetadata,
    onLoadedData,
    onPlay,
    onPause,
    onStalled,
    onWaiting,
    onProgress,
  }
  if (kind === 'audio') {
    return (
      <audio
        ref={elementRef as Ref<HTMLAudioElement> | undefined}
        class="media-player"
        controls
        preload="metadata"
        src={src}
        {...handlers}
      >
        <track kind="captions" />
      </audio>
    )
  }
  const size = videoSizeStyle(width, height)
  return (
    <video
      ref={elementRef as Ref<HTMLVideoElement> | undefined}
      class={size === null ? 'lightbox-player' : 'lightbox-player sized'}
      style={size ?? undefined}
      controls
      autoPlay
      playsInline
      preload="metadata"
      poster={poster}
      src={src}
      {...handlers}
    >
      <track kind="captions" />
    </video>
  )
}
