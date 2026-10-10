import type { MemberDto } from '../stores/room-list'
import { UserAvatar } from './UserAvatar'

export function userInfoLabel(displayName: string): string {
  return `User information for ${displayName}`
}

/**
 * Sender name on a timeline row, a reply quote, or a compact thread root.
 * Without `onOpenUser` it stays the plain label those rows always had.
 */
export function UserOpenName({
  userId,
  displayName,
  onOpenUser,
}: {
  userId: string
  displayName: string
  onOpenUser?: (userId: string) => void
}) {
  if (onOpenUser === undefined) {
    return <span class="event-sender">{displayName}</span>
  }
  return (
    <button
      type="button"
      class="event-sender"
      aria-label={userInfoLabel(displayName)}
      onClick={(event) => {
        event.stopPropagation()
        onOpenUser(userId)
      }}
    >
      {displayName}
    </button>
  )
}

/**
 * Avatar beside that name. The button is the accessible control: `UserAvatar`
 * itself stays presentational.
 */
export function UserOpenAvatar({
  accountId,
  userId,
  displayName,
  member,
  onOpenUser,
}: {
  accountId: string
  userId: string
  displayName: string
  member: MemberDto | undefined
  onOpenUser?: (userId: string) => void
}) {
  const avatar = (
    <UserAvatar
      accountId={accountId}
      userId={userId}
      displayName={displayName}
      member={member}
    />
  )
  if (onOpenUser === undefined) {
    return avatar
  }
  return (
    <button
      type="button"
      class="event-person"
      aria-label={userInfoLabel(displayName)}
      onClick={(event) => {
        event.stopPropagation()
        onOpenUser(userId)
      }}
    >
      {avatar}
    </button>
  )
}
