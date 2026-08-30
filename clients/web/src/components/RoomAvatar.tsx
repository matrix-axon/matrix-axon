import { useMediaBlob } from '../media/use-media-blob'

export function roomAvatarLabel(title: string | null | undefined): string {
  const trimmed = (title ?? '').trim()
  return trimmed === '' ? '?' : trimmed[0].toLocaleUpperCase()
}

export function roomAvatarColor(roomKeyValue: string): number {
  let hash = 0
  for (let index = 0; index < roomKeyValue.length; index += 1) {
    hash = (hash * 31 + roomKeyValue.charCodeAt(index)) >>> 0
  }
  return hash % 8
}

export function RoomAvatar({
  accountId,
  mxcUrl,
  title,
  color,
  previewUrl = null,
}: {
  accountId: string
  mxcUrl: string | null
  title: string
  color: number
  /**
   * A local object URL to show instead of the resolved `mxcUrl` — the image
   * the user just picked, before the write has round-tripped through sync.
   * When set, no media fetch is started; the caller owns revoking the URL.
   */
  previewUrl?: string | null
}) {
  const { ref, state } = useMediaBlob<HTMLSpanElement>(
    accountId,
    previewUrl === null ? mxcUrl : null,
  )
  const label = roomAvatarLabel(title)
  const shown =
    previewUrl ??
    (state.status === 'ready' && state.url !== undefined ? state.url : null)
  return (
    <span
      ref={ref}
      class={`room-avatar room-avatar-color-${color}`}
      aria-hidden="true"
      title={shown === null ? undefined : `${title} avatar`}
    >
      {shown !== null ? <img src={shown} alt="" /> : <span>{label}</span>}
    </span>
  )
}
