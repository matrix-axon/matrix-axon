import { useLocation } from 'preact-iso'
import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { inBackground } from '../api/client'
import {
  localRoomHref,
  matrixToRoomReferenceLink,
  serverNamesFromUserId,
  serverNameFromRoomReference,
} from '../matrix-to'
import { parseUserIdList } from '../matrix-user'
import { useServices } from '../services'
import type { MembersStore } from '../stores/members'
import {
  memberDisplay,
  roomKey,
  roomTitle,
  type MemberDto,
  type RoomDto,
} from '../stores/room-list'
import { useShortcuts } from '../shortcuts'
import { formatInviteeList, inviteErrorMessage } from '../invite'
import { BodyPortal } from './BodyPortal'
import { ErrorBanner } from './ErrorBanner'
import { useModalFocus } from './use-modal-focus'
import { UserAvatar } from './UserAvatar'

const MEMBERSHIP_ORDER = new Map([
  ['join', 0],
  ['invite', 1],
  ['leave', 2],
  ['ban', 3],
])

export function RoomInfoPanel({
  accountId,
  roomId,
  room,
  roomTitles,
  members,
  onClose,
}: {
  accountId: string
  roomId: string
  room: RoomDto | undefined
  roomTitles: ReadonlyMap<string, string>
  members: MembersStore
  onClose: () => void
}) {
  const location = useLocation()
  const { rooms, search } = useServices()
  const inviteInput = useRef<HTMLInputElement>(null)
  const clearCopyStatus = useRef<number | null>(null)
  const [filter, setFilter] = useState('')
  const [dmUserId, setDmUserId] = useState<string | null>(null)
  const [dmError, setDmError] = useState<string | null>(null)
  const [copyStatus, setCopyStatus] = useState<'idle' | 'copied' | 'failed'>(
    'idle',
  )
  const [inviteOpen, setInviteOpen] = useState(false)
  const [inviteValue, setInviteValue] = useState('')
  const [inviteStatus, setInviteStatus] = useState<string | null>(null)
  const [inviting, setInviting] = useState(false)
  const [cancelInviteMember, setCancelInviteMember] =
    useState<MemberDto | null>(null)
  const [cancelInviteBusy, setCancelInviteBusy] = useState(false)
  const [cancelInviteStatus, setCancelInviteStatus] = useState<string | null>(
    null,
  )
  const [leaveStatus, setLeaveStatus] = useState<string | null>(null)
  const [leaveConfirmOpen, setLeaveConfirmOpen] = useState(false)
  const [leaveBusy, setLeaveBusy] = useState(false)
  const displayTitle = room !== undefined ? roomTitle(room, roomTitles) : roomId
  const ownUserId = room?.account_user_id ?? null
  const homeServerName = serverNameFromRoomReference(ownUserId ?? roomId ?? '')
  const dmTitle =
    room !== undefined && roomTitles.has(roomKey(room))
      ? (roomTitles.get(roomKey(room)) ?? null)
      : null
  const roster = useMemo(
    () => filteredMembers([...members.members.value.values()], filter),
    [members.members.value, filter],
  )
  const onlyJoinedMember =
    ownUserId !== null &&
    isOnlyJoinedMember(members.members.value.values(), ownUserId)

  useEffect(() => {
    if (inviteOpen) {
      inviteInput.current?.focus()
    }
  }, [inviteOpen])
  useEffect(() => {
    return () => {
      if (clearCopyStatus.current !== null) {
        window.clearTimeout(clearCopyStatus.current)
      }
    }
  }, [])

  const copyRoomLink = async () => {
    if (clearCopyStatus.current !== null) {
      window.clearTimeout(clearCopyStatus.current)
      clearCopyStatus.current = null
    }
    const href = matrixToRoomReferenceLink(
      room?.room_id ?? roomId,
      room?.canonical_alias,
      { via: serverNamesFromUserId(room?.account_user_id) },
    )
    const ok = await copyText(href)
    setCopyStatus(ok ? 'copied' : 'failed')
    clearCopyStatus.current = window.setTimeout(() => {
      setCopyStatus('idle')
      clearCopyStatus.current = null
    }, 1800)
  }

  const startDm = async (userId: string) => {
    if (dmUserId !== null) {
      return
    }
    setDmError(null)
    setDmUserId(userId)
    const result = await rooms.createDm(accountId, userId)
    setDmUserId(null)
    if (!result.ok) {
      setDmError(`Could not start DM with ${userId}: ${result.message}`)
      return
    }
    location.route(localRoomHref(accountId, result.roomId, null))
    onClose()
  }
  const submitInvite = async (event: Event) => {
    event.preventDefault()
    setInviteStatus(null)
    const invite = parseUserIdList(inviteValue, homeServerName)
    if (!invite.ok) {
      setInviteStatus(invite.message)
      return
    }
    if (invite.userIds.length === 0) {
      setInviteStatus('Enter at least one Matrix user ID to invite.')
      return
    }
    if (
      ownUserId !== null &&
      invite.userIds.some((userId) => userId === ownUserId)
    ) {
      setInviteStatus('Invite another user, not yourself.')
      return
    }
    setInviting(true)
    const result = await members.inviteUsers(invite.userIds)
    setInviting(false)
    if (!result.ok) {
      setInviteStatus(inviteErrorMessage(result))
      return
    }
    setInviteValue('')
    search.clear()
    setInviteStatus(`Invited ${formatInviteeList(invite.userIds)}.`)
  }

  const confirmCancelInvite = async () => {
    if (cancelInviteMember === null) {
      return
    }
    const member = cancelInviteMember
    setCancelInviteStatus(null)
    setCancelInviteBusy(true)
    const result = await members.cancelInvite(member.user_id)
    setCancelInviteBusy(false)
    if (!result.ok) {
      setCancelInviteMember(null)
      setCancelInviteStatus(
        `Could not cancel invite for ${memberDisplay(member)}: ${result.message}`,
      )
      return
    }
    setCancelInviteMember(null)
    setCancelInviteStatus(`Canceled invite for ${memberDisplay(member)}.`)
  }

  const confirmLeave = async () => {
    if (ownUserId === null) {
      setLeaveStatus('Room membership is still loading; try again.')
      return
    }
    setLeaveStatus(null)
    setLeaveBusy(true)
    await members.refresh()
    setLeaveBusy(false)
    if (members.error.value !== null) {
      setLeaveStatus(`Could not refresh room members: ${members.error.value}`)
      return
    }
    setLeaveConfirmOpen(true)
  }

  const leaveRoom = async () => {
    setLeaveBusy(true)
    const result = await rooms.leaveRoom(accountId, roomId)
    setLeaveBusy(false)
    if (!result.ok) {
      setLeaveConfirmOpen(false)
      setLeaveStatus(`Could not leave room: ${result.message}`)
      return
    }
    location.route('/', true)
  }

  return (
    <aside
      id="room-info-panel"
      class="side-panel room-info-panel"
      aria-label="Room information"
    >
      <div class="overlay-head">
        <h2>Room information</h2>
        <button type="button" class="ghost" onClick={onClose}>
          Close
        </button>
      </div>

      <section class="room-info-section" aria-labelledby="room-info-details">
        <h3 id="room-info-details">Details</h3>
        <dl class="detail-list">
          <DetailRow label="Name" value={displayTitle} />
          {dmTitle !== null && dmTitle !== displayTitle && (
            <DetailRow label="DM name" value={dmTitle} />
          )}
          <DetailRow label="Room ID" value={room?.room_id ?? roomId} code />
          <DetailRow
            label="Account ID"
            value={room?.account_id ?? accountId}
            code
          />
          <DetailRow
            label="Your Matrix ID"
            value={room?.account_user_id ?? 'Unavailable from room summary'}
            code={room?.account_user_id !== undefined}
          />
          <DetailRow
            label="Canonical alias"
            value={room?.canonical_alias ?? 'None'}
            code={
              room?.canonical_alias !== undefined &&
              room.canonical_alias !== null
            }
          />
          <DetailRow
            label="Full alias list"
            value="Unavailable from current API"
          />
          <DetailRow label="Topic" value={room?.topic ?? 'None'} />
          <DetailRow label="Avatar" value={room?.avatar_url ?? 'None'} code />
          <DetailRow
            label="Last activity"
            value={
              room !== undefined
                ? new Date(room.last_activity_ts).toLocaleString()
                : 'Unavailable from room summary'
            }
          />
          <DetailRow
            label="Last event"
            value={room?.last_event_id ?? 'None'}
            code={
              room?.last_event_id !== undefined && room.last_event_id !== null
            }
          />
          <DetailRow label="Encryption" value="Unavailable from current API" />
          <DetailRow label="Access" value="Unavailable from current API" />
          <DetailRow
            label="Room type/version"
            value="Unavailable from current API"
          />
        </dl>
      </section>

      <section class="room-info-section" aria-labelledby="room-info-actions">
        <h3 id="room-info-actions">Actions</h3>
        <div class="room-info-actions">
          <button
            type="button"
            class="ghost"
            onClick={() => void copyRoomLink()}
          >
            Copy link
          </button>
          <button
            type="button"
            onClick={() => {
              setInviteOpen((open) => !open)
              setInviteStatus(null)
            }}
          >
            Invite
          </button>
          <button
            type="button"
            class="danger room-info-leave"
            disabled={leaveBusy}
            onClick={() => void confirmLeave()}
          >
            {leaveBusy ? 'Checking…' : 'Leave'}
          </button>
        </div>
        {copyStatus !== 'idle' && (
          <p
            class={`room-info-status${copyStatus === 'failed' ? ' error' : ''}`}
            role="status"
          >
            {copyStatus === 'copied' ? 'Copied' : 'Could not copy link'}
          </p>
        )}
        {leaveStatus !== null && (
          <p class="error" role="alert">
            {leaveStatus}
          </p>
        )}
        {inviteOpen && (
          <form class="room-invite-form" onSubmit={submitInvite}>
            <label>
              Invite people
              <input
                ref={inviteInput}
                type="text"
                value={inviteValue}
                placeholder="@alice:example.org, @bob:example.org"
                autocapitalize="none"
                autocorrect="off"
                spellcheck={false}
                inputmode="email"
                onInput={(event) => {
                  setInviteValue(event.currentTarget.value)
                  setInviteStatus(null)
                }}
              />
            </label>
            <button type="submit" disabled={inviting}>
              {inviting ? 'Inviting…' : 'Send invite'}
            </button>
          </form>
        )}
        {inviteStatus !== null && (
          <p
            class={
              inviteStatus.startsWith('Invited ') ? 'room-info-status' : 'error'
            }
            role={inviteStatus.startsWith('Invited ') ? 'status' : 'alert'}
          >
            {inviteStatus}
          </p>
        )}
      </section>

      <section class="room-info-section" aria-labelledby="room-info-members">
        <div class="room-info-section-head">
          <h3 id="room-info-members">Members</h3>
          <button
            type="button"
            class="ghost"
            disabled={members.loading.value}
            onClick={() => inBackground(members.refresh())}
          >
            {members.loading.value ? 'Refreshing…' : 'Refresh'}
          </button>
        </div>
        <ErrorBanner error={members.error} />
        <label class="member-filter">
          Filter members
          <input
            type="search"
            value={filter}
            placeholder="Name, MXID, membership"
            onInput={(event) => setFilter(event.currentTarget.value)}
          />
        </label>
        {dmError !== null && (
          <p class="error" role="alert">
            {dmError}
          </p>
        )}
        {cancelInviteStatus !== null && (
          <p
            class={
              cancelInviteStatus.startsWith('Canceled ')
                ? 'room-info-status'
                : 'error'
            }
            role={
              cancelInviteStatus.startsWith('Canceled ') ? 'status' : 'alert'
            }
          >
            {cancelInviteStatus}
          </p>
        )}
        {members.loading.value && members.members.value.size === 0 ? (
          <p class="muted">Loading members…</p>
        ) : roster.length === 0 ? (
          <p class="muted">
            {filter.trim() === ''
              ? 'No members available.'
              : 'No members match.'}
          </p>
        ) : (
          <ol class="member-list">
            {roster.map((member) => {
              const display = memberDisplay(member)
              const dmLabel = `Open DM with ${display} (${member.user_id})`
              return (
                <li class="member-row" key={member.user_id}>
                  {member.user_id === ownUserId ? (
                    <span class="member-person">
                      <UserAvatar
                        accountId={accountId}
                        userId={member.user_id}
                        displayName={display}
                        member={member}
                      />
                      <span class="member-copy">
                        <span class="member-name">{display}</span>
                        <code>{member.user_id}</code>
                      </span>
                    </span>
                  ) : (
                    <button
                      type="button"
                      class="member-person member-identity-action"
                      disabled={dmUserId !== null}
                      aria-label={dmLabel}
                      title={dmLabel}
                      onClick={() => void startDm(member.user_id)}
                    >
                      <UserAvatar
                        accountId={accountId}
                        userId={member.user_id}
                        displayName={display}
                        member={member}
                      />
                      <span class="member-copy">
                        <span class="member-name">{display}</span>
                        <code>{member.user_id}</code>
                      </span>
                    </button>
                  )}
                  {member.membership === 'invite' ? (
                    <button
                      type="button"
                      class="badge membership-invite membership-action"
                      disabled={cancelInviteBusy}
                      aria-label={`Cancel invite for ${display} (${member.user_id})`}
                      title={`Cancel invite for ${display} (${member.user_id})`}
                      onClick={() => {
                        setCancelInviteStatus(null)
                        setCancelInviteMember(member)
                      }}
                    >
                      Invited
                    </button>
                  ) : (
                    <span class={`badge membership-${member.membership}`}>
                      {membershipLabel(member.membership)}
                    </span>
                  )}
                </li>
              )
            })}
          </ol>
        )}
      </section>
      {leaveConfirmOpen && (
        <LeaveRoomDialog
          roomTitle={displayTitle}
          onlyJoinedMember={onlyJoinedMember}
          busy={leaveBusy}
          onCancel={() => {
            if (!leaveBusy) {
              setLeaveConfirmOpen(false)
            }
          }}
          onConfirm={() => void leaveRoom()}
        />
      )}
      {cancelInviteMember !== null && (
        <CancelInviteDialog
          member={cancelInviteMember}
          busy={cancelInviteBusy}
          onCancel={() => {
            if (!cancelInviteBusy) {
              setCancelInviteMember(null)
            }
          }}
          onConfirm={() => void confirmCancelInvite()}
        />
      )}
    </aside>
  )
}

function CancelInviteDialog({
  member,
  busy,
  onCancel,
  onConfirm,
}: {
  member: MemberDto
  busy: boolean
  onCancel: () => void
  onConfirm: () => void
}) {
  const { containerRef } = useModalFocus<HTMLDivElement>()
  const display = memberDisplay(member)
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onCancel()
      },
    },
    { whileTyping: true, capture: true },
  )
  return (
    <BodyPortal>
      <div
        ref={containerRef}
        class="overlay"
        role="dialog"
        aria-modal="true"
        aria-label={`Cancel invite for ${display}`}
      >
        <div class="overlay-panel cancel-invite-dialog">
          <h2>Cancel invite?</h2>
          <p>
            Cancel the pending invite for {display} ({member.user_id})?
          </p>
          <div class="dialog-actions">
            <button
              type="button"
              class="ghost"
              disabled={busy}
              onClick={onCancel}
            >
              Keep invite
            </button>
            <button
              type="button"
              class="danger"
              disabled={busy}
              onClick={onConfirm}
            >
              {busy ? 'Canceling…' : 'Cancel invite'}
            </button>
          </div>
        </div>
      </div>
    </BodyPortal>
  )
}

function LeaveRoomDialog({
  roomTitle,
  onlyJoinedMember,
  busy,
  onCancel,
  onConfirm,
}: {
  roomTitle: string
  onlyJoinedMember: boolean
  busy: boolean
  onCancel: () => void
  onConfirm: () => void
}) {
  const { containerRef } = useModalFocus<HTMLDivElement>()
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onCancel()
      },
    },
    { whileTyping: true, capture: true },
  )
  return (
    <BodyPortal>
      <div
        ref={containerRef}
        class="overlay"
        role="dialog"
        aria-modal="true"
        aria-label={`Leave ${roomTitle}`}
      >
        <div class="overlay-panel leave-room-dialog">
          <h2>Leave {roomTitle}?</h2>
          <p>
            {onlyJoinedMember
              ? 'You are the only joined member in this room. If you leave, there may be no one left to invite you back.'
              : 'You will stop receiving messages from this room. You may need another invite to return.'}
          </p>
          <div class="dialog-actions">
            <button
              type="button"
              class="ghost"
              disabled={busy}
              onClick={onCancel}
            >
              Cancel
            </button>
            <button
              type="button"
              class="danger"
              disabled={busy}
              onClick={onConfirm}
            >
              {busy ? 'Leaving…' : 'Leave room'}
            </button>
          </div>
        </div>
      </div>
    </BodyPortal>
  )
}

function DetailRow({
  label,
  value,
  code = false,
}: {
  label: string
  value: string
  code?: boolean
}) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{code ? <code>{value}</code> : value}</dd>
    </>
  )
}

function filteredMembers(members: MemberDto[], filter: string): MemberDto[] {
  const query = filter.trim().toLocaleLowerCase()
  return members
    .filter((member) => {
      if (query === '') {
        return true
      }
      return [memberDisplay(member), member.user_id, member.membership].some(
        (field) => field.toLocaleLowerCase().includes(query),
      )
    })
    .sort((left, right) => {
      const leftRank = MEMBERSHIP_ORDER.get(left.membership) ?? 99
      const rightRank = MEMBERSHIP_ORDER.get(right.membership) ?? 99
      if (leftRank !== rightRank) {
        return leftRank - rightRank
      }
      return (
        memberDisplay(left).localeCompare(memberDisplay(right), undefined, {
          sensitivity: 'base',
        }) || left.user_id.localeCompare(right.user_id)
      )
    })
}

function membershipLabel(membership: string): string {
  switch (membership) {
    case 'join':
      return 'Joined'
    case 'invite':
      return 'Invited'
    case 'leave':
      return 'Left'
    case 'ban':
      return 'Banned'
    default:
      return membership || 'Unknown'
  }
}

function isOnlyJoinedMember(
  members: Iterable<MemberDto>,
  ownUserId: string,
): boolean {
  const joined = [...members].filter((member) => member.membership === 'join')
  return joined.length === 1 && joined[0]?.user_id === ownUserId
}

async function copyText(text: string): Promise<boolean> {
  if (navigator.clipboard?.writeText === undefined) {
    return false
  }
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}
