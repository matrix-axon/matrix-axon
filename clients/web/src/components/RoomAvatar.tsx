import { useMediaBlob } from '../media/use-media-blob'

export function roomAvatarLabel(title: string): string {
  const trimmed = title.trim()
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
}: {
  accountId: string
  mxcUrl: string | null
  title: string
  color: number
}) {
  const { ref, state } = useMediaBlob<HTMLSpanElement>(accountId, mxcUrl)
  const label = roomAvatarLabel(title)
  return (
    <span
      ref={ref}
      class={`room-avatar room-avatar-color-${color}`}
      aria-hidden="true"
      title={mxcUrl === null ? undefined : `${title} avatar`}
    >
      {state.status === 'ready' && state.url !== undefined ? (
        <img src={state.url} alt="" />
      ) : (
        <span>{label}</span>
      )}
    </span>
  )
}
