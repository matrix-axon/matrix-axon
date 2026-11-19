import type { Ref } from 'preact'

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
}: {
  kind: 'audio' | 'video'
  src: string
  /** Video only: the sender's thumbnail, as an object URL. */
  poster?: string
  elementRef?: Ref<HTMLMediaElement>
  onError: () => void
  onLoadedMetadata?: () => void
  onLoadedData?: () => void
  onPlay?: () => void
  onPause?: () => void
  onStalled?: () => void
  onWaiting?: () => void
}) {
  const handlers = {
    onError,
    onLoadedMetadata,
    onLoadedData,
    onPlay,
    onPause,
    onStalled,
    onWaiting,
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
  return (
    <video
      ref={elementRef as Ref<HTMLVideoElement> | undefined}
      class="lightbox-player"
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
