import { useLocation } from 'preact-iso'
import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { inBackground } from '../api/client'
import { timelineEvent } from '../api/frames'
import type { components } from '../api/schema'
import {
  localRoomHref,
  matrixToRoomReferenceLink,
  serverNamesFromUserId,
  serverNameFromRoomReference,
} from '../matrix-to'
import { parseUserIdList } from '../matrix-user'
import { joinRoomError } from '../room-entry'
import { useServices } from '../services'
import type { MembersStore } from '../stores/members'
import {
  memberDisplay,
  roomKey,
  roomListAvatarUrl,
  roomTitle,
  type MemberDto,
  type RoomDto,
} from '../stores/room-list'
import { useShortcuts } from '../shortcuts'
import { formatInviteeList, inviteErrorMessage } from '../invite'
import { BodyPortal } from './BodyPortal'
import { CopyableText, useCopyFeedback } from './CopyableText'
import { RoomAvatar, roomAvatarColor } from './RoomAvatar'
import { ErrorBanner } from './ErrorBanner'
import { useModalFocus } from './use-modal-focus'
import { UserAvatar } from './UserAvatar'

type RoomInfoDto = components['schemas']['RoomInfoDto']
type RoomUpgradeDto = components['schemas']['RoomUpgradeDto']
type SpaceChildDto = components['schemas']['SpaceChildDto']
type SpaceParentDto = components['schemas']['SpaceParentDto']
type EventDto = components['schemas']['EventDto']

type RoomStateKey = 'info' | 'pinned' | 'children' | 'parents' | 'upgrade'

interface RoomStateSlice {
  /** `accountId/roomId` the rest of this slice was loaded for. */
  key: string
  info: RoomInfoDto | null
  pinned: readonly EventDto[] | null
  children: readonly SpaceChildDto[] | null
  parents: readonly SpaceParentDto[] | null
  upgrade: RoomUpgradeDto | null
  errors: Partial<Record<RoomStateKey, string>>
}

function emptyRoomState(key: string): RoomStateSlice {
  return {
    key,
    info: null,
    pinned: null,
    children: null,
    parents: null,
    upgrade: null,
    errors: {},
  }
}

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
  const { rooms, search, spaces, api, live } = useServices()
  const inviteInput = useRef<HTMLInputElement>(null)
  const { status: copyStatus, copy } = useCopyFeedback()
  const [filter, setFilter] = useState('')
  const [dmUserId, setDmUserId] = useState<string | null>(null)
  const [dmError, setDmError] = useState<string | null>(null)
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
  const [joinLink, setJoinLink] = useState<{
    roomId: string
    via: readonly string[]
    label: string
  } | null>(null)
  const [joinLinkBusy, setJoinLinkBusy] = useState(false)
  const [joinLinkStatus, setJoinLinkStatus] = useState<string | null>(null)
  const [leaveStatus, setLeaveStatus] = useState<string | null>(null)
  const [leaveConfirmOpen, setLeaveConfirmOpen] = useState(false)
  const [leaveBusy, setLeaveBusy] = useState(false)
  // `RoomPage` deliberately keeps one panel instance across room navigation, so
  // this state has to be keyed by room itself: without it, switching rooms with
  // the drawer open leaves the previous room's encryption, access and pinned
  // messages on screen until the new fetches land — and indefinitely if they
  // fail. The key is compared during render so no stale frame is ever shown.
  const roomStateKey = `${accountId}/${roomId}`
  const [loadedState, setRoomState] = useState<RoomStateSlice>(() =>
    emptyRoomState(roomStateKey),
  )
  const roomState =
    loadedState.key === roomStateKey
      ? loadedState
      : emptyRoomState(roomStateKey)
  /** Room-state details, with a failed `/info` reported rather than pending. */
  const infoValue = (pick: (info: RoomInfoDto) => string) => {
    if (roomState.info !== null) return pick(roomState.info)
    return roomState.errors.info === undefined ? 'Loading…' : 'Unavailable'
  }
  const [roomStateVersion, setRoomStateVersion] = useState(0)
  const displayTitle =
    room !== undefined ? roomTitle(room, roomTitles) || roomId : roomId
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
  const parents = useMemo(
    () =>
      relatedSpaceParents(
        roomState.parents,
        spaces.children.value,
        rooms.rooms.value,
        accountId,
        roomId,
        roomTitles,
      ),
    [
      roomState.parents,
      spaces.children.value,
      rooms.rooms.value,
      accountId,
      roomId,
      roomTitles,
    ],
  )

  useEffect(() => {
    if (inviteOpen) {
      inviteInput.current?.focus()
    }
  }, [inviteOpen])
  useEffect(() => {
    let cancelled = false
    const stateKey = `${accountId}/${roomId}`
    // Writes always land on the slice for the room they were issued for, never
    // on whatever room the panel has since moved to.
    const forRoom = (current: RoomStateSlice) =>
      current.key === stateKey ? current : emptyRoomState(stateKey)
    const load = <T,>(
      key: RoomStateKey,
      request: Promise<{ data?: { data: T } }>,
    ) => {
      const fail = () =>
        !cancelled &&
        setRoomState((current) => ({
          ...forRoom(current),
          errors: {
            ...forRoom(current).errors,
            [key]: 'Could not load this section.',
          },
        }))
      void request.then(({ data }) => {
        if (cancelled) return
        if (data === undefined) {
          fail()
          return
        }
        setRoomState((current) => ({
          ...forRoom(current),
          [key]: data.data,
          errors: { ...forRoom(current).errors, [key]: undefined },
        }))
      }, fail)
    }
    const params = {
      params: { path: { account_id: accountId, room_id: roomId } },
    }
    load(
      'info',
      api.GET('/v1/accounts/{account_id}/rooms/{room_id}/info', params),
    )
    load(
      'pinned',
      api.GET('/v1/accounts/{account_id}/rooms/{room_id}/pinned', params),
    )
    load(
      'children',
      api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/space/children',
        params,
      ),
    )
    load(
      'parents',
      api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/space/parents',
        params,
      ),
    )
    load(
      'upgrade',
      api.GET('/v1/accounts/{account_id}/rooms/{room_id}/upgrade', params),
    )
    return () => {
      cancelled = true
    }
  }, [api, accountId, roomId, roomStateVersion])
  useEffect(() => {
    return live.subscribe((frame) => {
      const event = timelineEvent(frame)
      if (
        event !== null &&
        event.account_id === accountId &&
        event.room_id === roomId &&
        [
          'm.room.join_rules',
          'm.room.history_visibility',
          'm.room.guest_access',
          'm.room.encryption',
          'm.room.pinned_events',
          'm.space.child',
          'm.space.parent',
          'm.room.tombstone',
          'm.room.create',
        ].includes(event.type)
      ) {
        setRoomStateVersion((version) => version + 1)
      }
    })
  }, [live, accountId, roomId])
  useEffect(() => {
    if (live.reconnects.value > 0) {
      setRoomStateVersion((version) => version + 1)
    }
  }, [live.reconnects.value])

  const copyRoomLink = async () => {
    const href = matrixToRoomReferenceLink(
      room?.room_id ?? roomId,
      room?.canonical_alias,
      { via: serverNamesFromUserId(room?.account_user_id) },
    )
    await copy(href)
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

  const joinLinkedRoom = async (target: {
    roomId: string
    via: readonly string[]
    label: string
  }) => {
    setJoinLinkBusy(true)
    const result = await rooms.joinRoom(accountId, target.roomId, target.via)
    setJoinLinkBusy(false)
    setJoinLink(null)
    if (!result.ok) {
      setJoinLinkStatus(joinRoomError(target.label, result))
      return
    }
    location.route(localRoomHref(accountId, result.roomId, null))
    onClose()
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

      <div class="room-info-identity">
        <RoomAvatar
          accountId={accountId}
          mxcUrl={
            room !== undefined
              ? roomListAvatarUrl(room, rooms.dmAvatars.value)
              : null
          }
          title={displayTitle}
          color={roomAvatarColor(roomStateKey)}
        />
        <div class="room-info-identity-copy">
          <p class="room-info-identity-name">{displayTitle}</p>
          {room?.topic !== undefined &&
            room.topic !== null &&
            room.topic.trim() !== '' && (
              <p class="room-info-identity-topic muted">{room.topic}</p>
            )}
        </div>
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
          <DetailRow
            label="Encryption"
            value={infoValue(
              (info) => info.encryption_algorithm ?? 'Unencrypted',
            )}
          />
          <DetailRow
            label="Access"
            value={infoValue((info) => info.join_rule ?? 'Unavailable')}
          />
          <DetailRow
            label="History visibility"
            value={infoValue(
              (info) => info.history_visibility ?? 'Unavailable',
            )}
          />
          <DetailRow
            label="Guest access"
            value={infoValue((info) => info.guest_access ?? 'Unavailable')}
          />
          <DetailRow label="Room type" value={room?.room_type ?? 'None'} />
        </dl>
        {roomState.errors.info !== undefined && (
          <p class="error" role="alert">
            {roomState.errors.info}
          </p>
        )}
      </section>

      <RoomStateLinks
        children={roomState.children}
        parents={parents}
        upgrade={roomState.upgrade}
        errors={roomState.errors}
        status={joinLinkStatus}
        onOpen={(target, via, label) => {
          const known = rooms.rooms.value.find(
            (candidate) =>
              candidate.account_id === accountId &&
              candidate.room_id === target,
          )
          if (known !== undefined) {
            location.route(localRoomHref(accountId, target, null))
            onClose()
            return
          }
          // Opening an unjoined relation means joining it — a membership
          // change the button label does not imply, so it is confirmed first.
          setJoinLinkStatus(null)
          setJoinLink({ roomId: target, via, label })
        }}
      />

      <section class="room-info-section" aria-labelledby="room-info-pinned">
        <h3 id="room-info-pinned">Pinned messages</h3>
        {roomState.errors.pinned !== undefined ? (
          <p class="error" role="alert">
            {roomState.errors.pinned}
          </p>
        ) : roomState.pinned === null ? (
          <p class="muted">Loading pinned messages…</p>
        ) : roomState.pinned.length === 0 ? (
          <p class="muted">No pinned messages.</p>
        ) : (
          <ol class="pinned-message-list">
            {roomState.pinned.map((event) => (
              <li key={event.event_id}>
                <a href={localRoomHref(accountId, roomId, event.event_id)}>
                  {event.sender}: {eventSummary(event)}
                </a>
              </li>
            ))}
          </ol>
        )}
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
      {joinLink !== null && (
        <JoinLinkedRoomDialog
          label={joinLink.label}
          roomId={joinLink.roomId}
          busy={joinLinkBusy}
          onCancel={() => {
            if (!joinLinkBusy) setJoinLink(null)
          }}
          onConfirm={() => void joinLinkedRoom(joinLink)}
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

/**
 * Matrix spaces normally establish membership with an `m.space.child` event
 * in the parent space. `m.space.parent` in the child is optional, so augment
 * the endpoint's direct projection with the joined spaces already loaded for
 * the picker.
 */
function relatedSpaceParents(
  directParents: readonly SpaceParentDto[] | null,
  childrenBySpace: ReadonlyMap<string, readonly SpaceChildDto[]>,
  rooms: readonly RoomDto[],
  accountId: string,
  roomId: string,
  roomTitles: ReadonlyMap<string, string>,
): readonly SpaceParentDto[] | null {
  const parents = new Map(
    (directParents ?? []).map((parent) => [parent.room_id, parent]),
  )
  for (const space of rooms) {
    if (space.account_id !== accountId || space.room_type !== 'm.space')
      continue
    const child = childrenBySpace
      .get(roomKey(space))
      ?.find((candidate) => candidate.room_id === roomId)
    if (child !== undefined && !parents.has(space.room_id)) {
      parents.set(space.room_id, {
        room_id: space.room_id,
        room_type: space.room_type,
        name: roomTitle(space, roomTitles),
        canonical: false,
        via: child.via,
      })
    }
  }
  return directParents === null && parents.size === 0
    ? null
    : [...parents.values()]
}

function RoomStateLinks({
  children,
  parents,
  upgrade,
  errors,
  status,
  onOpen,
}: {
  children: readonly SpaceChildDto[] | null
  parents: readonly SpaceParentDto[] | null
  upgrade: RoomUpgradeDto | null
  errors: Partial<Record<RoomStateKey, string>>
  status: string | null
  onOpen: (roomId: string, via: readonly string[], label: string) => void
}) {
  const links = [
    ...(parents ?? []).map((parent) => ({
      label: `Parent: ${parent.name ?? parent.room_id}`,
      roomId: parent.room_id,
      via: parent.via,
    })),
    ...(children ?? []).map((child) => ({
      label: `Child: ${child.name ?? child.room_id}`,
      roomId: child.room_id,
      via: child.via,
    })),
    ...(upgrade?.upgraded_from === null || upgrade?.upgraded_from === undefined
      ? []
      : [{ label: 'Upgraded from', roomId: upgrade.upgraded_from, via: [] }]),
    ...(upgrade?.tombstoned_to === null || upgrade?.tombstoned_to === undefined
      ? []
      : [
          {
            label: 'Open replacement room',
            roomId: upgrade.tombstoned_to,
            via: [],
          },
        ]),
  ]
  const failed = (['children', 'parents', 'upgrade'] as const).filter(
    (key) => errors[key] !== undefined,
  )
  // Nothing loaded *and* nothing failed is the only genuinely pending case. If
  // every read failed — the offline case — this used to take the same branch
  // and sit at "Loading…" forever with the errors suppressed.
  if (
    links.length === 0 &&
    children === null &&
    parents === null &&
    upgrade === null &&
    failed.length === 0
  ) {
    return (
      <section class="room-info-section">
        <h3>Spaces and upgrades</h3>
        <p class="muted">Loading room relationships…</p>
      </section>
    )
  }
  return (
    <section class="room-info-section" aria-labelledby="room-info-links">
      <h3 id="room-info-links">Spaces and upgrades</h3>
      {failed.map((key) => (
        <p class="error" role="alert" key={key}>
          {errors[key]}
        </p>
      ))}
      {links.length === 0 && failed.length === 0 ? (
        <p class="muted">No related spaces or upgrade links.</p>
      ) : links.length === 0 ? null : (
        <ul class="room-state-links">
          {links.map((link) => (
            <li key={`${link.label}:${link.roomId}`}>
              <button
                type="button"
                class="ghost"
                onClick={() => onOpen(link.roomId, link.via, link.label)}
              >
                {link.label}
              </button>
            </li>
          ))}
        </ul>
      )}
      {status !== null && (
        <p class="error" role="alert">
          {status}
        </p>
      )}
    </section>
  )
}

function eventSummary(event: EventDto): string {
  if (typeof event.content === 'object' && event.content !== null) {
    const body = (event.content as { body?: unknown }).body
    if (typeof body === 'string' && body.trim() !== '') return body
  }
  return event.type
}

/**
 * A relationship link to a room this account has not joined can only be opened
 * by joining it, which is a membership change the link's label does not imply.
 */
function JoinLinkedRoomDialog({
  label,
  roomId,
  busy,
  onCancel,
  onConfirm,
}: {
  label: string
  roomId: string
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
        aria-label={`Join ${label}`}
      >
        <div class="overlay-panel leave-room-dialog">
          <h2>Join this room?</h2>
          <p>
            You have not joined <strong>{label}</strong> (<code>{roomId}</code>
            ). Opening it joins the room with this account, which other members
            can see.
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
            <button type="button" disabled={busy} onClick={onConfirm}>
              {busy ? 'Joining…' : 'Join and open'}
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

/** Placeholders are not worth copying; real IDs, names, and state are. */
function isPopulatedDetail(value: string | null | undefined): value is string {
  return (
    typeof value === 'string' &&
    value !== '' &&
    value !== 'None' &&
    value !== 'Unavailable' &&
    value !== 'Loading…' &&
    !value.startsWith('Unavailable from')
  )
}

function DetailRow({
  label,
  value,
  code = false,
}: {
  label: string
  value: string | null | undefined
  code?: boolean
}) {
  const text = value ?? ''
  const displayed = code ? <code>{text}</code> : text
  return (
    <>
      <dt>{label}</dt>
      <dd>
        {isPopulatedDetail(value) ? (
          <CopyableText text={value} label={label}>
            {displayed}
          </CopyableText>
        ) : (
          displayed
        )}
      </dd>
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
