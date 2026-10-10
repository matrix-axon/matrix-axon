import type { MemberDto } from '../stores/room-list'
import { UserAvatar } from './UserAvatar'

/**
 * Sender name on a timeline row, a reply quote, or a compact thread root.
 * Without `onOpenUser` it stays the plain label those rows always had.
 *
 * The accessible name is the visible name. A separate "User information for…"
 * label hid the sender from the row's announcement.
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
 * Avatar beside that name. A pointer target only: the name is the one tab
 * stop, and `UserAvatar` stays presentational.
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
      tabIndex={-1}
      aria-hidden="true"
      onClick={(event) => {
        event.stopPropagation()
        onOpenUser(userId)
      }}
    >
      {avatar}
    </button>
  )
}
