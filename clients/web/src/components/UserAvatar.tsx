import { useMediaBlob } from '../media/use-media-blob'
import type { MemberDto } from '../stores/room-list'

function userAvatarLabel(displayName: string, userId: string): string {
  const trimmed = displayName.trim()
  const source =
    trimmed === '' || trimmed === userId
      ? (userId.replace(/^@/, '').split(':')[0] ?? '')
      : trimmed
  return source === '' ? '?' : source[0].toLocaleUpperCase()
}

function userAvatarColor(userId: string): number {
  let hash = 0
  for (let index = 0; index < userId.length; index += 1) {
    hash = (hash * 31 + userId.charCodeAt(index)) >>> 0
  }
  return hash % 8
}

export function UserAvatar({
  accountId,
  userId,
  displayName,
  member,
}: {
  accountId: string
  userId: string
  displayName: string
  member: MemberDto | undefined
}) {
  const mxcUrl = member?.avatar_url ?? null
  const { ref, state } = useMediaBlob<HTMLSpanElement>(accountId, mxcUrl)
  const color = userAvatarColor(userId)
  const label = userAvatarLabel(displayName, userId)
  const shown =
    state.status === 'ready' && state.url !== undefined ? state.url : null

  return (
    <span
      ref={ref}
      class={`user-avatar user-avatar-color-${color}${shown !== null ? ' user-avatar-image' : ''}`}
      aria-hidden="true"
      title={mxcUrl === null ? undefined : `${displayName} avatar`}
    >
      {shown !== null ? <img src={shown} alt="" /> : <span>{label}</span>}
    </span>
  )
}
