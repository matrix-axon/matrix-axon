import { useLocation } from 'preact-iso'
import { useEffect, useState } from 'preact/hooks'
import { apiErrorMessage } from '../api/client'
import type { components } from '../api/schema'
import { localRoomHref } from '../matrix-to'
import { useServices } from '../services'
import type { MembersStore } from '../stores/members'
import { memberDisplay, type MemberDto } from '../stores/room-list'
import { DetailRow } from './DetailRow'
import { Lightbox, LightboxImage } from './Lightbox'
import { UserAvatar } from './UserAvatar'

type MatrixProfileDto = components['schemas']['MatrixProfileDto']
type DeviceDto = components['schemas']['DeviceDto']

type Remote<T> =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'ready'; value: T }

function blankToNull(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? ''
  return trimmed === '' ? null : trimmed
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

function localTrustLabel(state: string): string {
  switch (state) {
    case 'verified':
      return 'Verified'
    case 'black_listed':
      return 'Blacklisted'
    case 'ignored':
      return 'Ignored'
    case 'unset':
      return 'Unset'
    default:
      return state
  }
}

/**
 * Avatar source for the header. A room membership row wins, including when it
 * has no picture: that is the profile the timeline already shows. The global
 * profile is only the fallback when this room has no row for the user.
 */
function headerMember(
  userId: string,
  roomMember: MemberDto | undefined,
  profile: MatrixProfileDto | null,
): MemberDto | undefined {
  if (roomMember !== undefined) {
    return roomMember
  }
  if (profile === null) {
    return undefined
  }
  const avatar = blankToNull(profile.avatar_url)
  const name = blankToNull(profile.display_name)
  if (avatar === null && name === null) {
    return undefined
  }
  return {
    user_id: userId,
    membership: 'leave',
    display_name: name,
    avatar_url: avatar,
  }
}

export function UserInfoPanel({
  accountId,
  userId,
  ownUserId,
  members,
  onClose,
}: {
  accountId: string
  userId: string
  ownUserId: string | null
  members: MembersStore
  onClose: () => void
}) {
  const location = useLocation()
  const { api, rooms, live } = useServices()
  const reconnects = live.reconnects.value
  const [profile, setProfile] = useState<Remote<MatrixProfileDto>>({
    status: 'loading',
  })
  const [devices, setDevices] = useState<Remote<readonly DeviceDto[]>>({
    status: 'loading',
  })
  const [avatarOpen, setAvatarOpen] = useState(false)
  const [startingDm, setStartingDm] = useState(false)
  const [dmError, setDmError] = useState<string | null>(null)

  // Membership and the room profile come from the room's members store, which
  // the room page refreshes on open and on reconnect. The global profile and
  // the device list are read here, again when the socket reconnects. An empty
  // device list means Axon has never tracked that user.
  useEffect(() => {
    let cancelled = false
    setProfile({ status: 'loading' })
    setDevices({ status: 'loading' })
    setAvatarOpen(false)
    void (async () => {
      try {
        const [profileResult, deviceResult] = await Promise.all([
          api.GET('/v1/accounts/{account_id}/users/{user_id}/profile', {
            params: { path: { account_id: accountId, user_id: userId } },
          }),
          api.GET('/v1/accounts/{account_id}/devices', {
            params: {
              path: { account_id: accountId },
              query: { user_id: userId },
            },
          }),
        ])
        if (cancelled) {
          return
        }
        if (
          profileResult.error !== undefined ||
          profileResult.data === undefined
        ) {
          setProfile({
            status: 'error',
            message: apiErrorMessage(profileResult.error),
          })
        } else {
          setProfile({ status: 'ready', value: profileResult.data.data })
        }
        if (
          deviceResult.error !== undefined ||
          deviceResult.data === undefined
        ) {
          setDevices({
            status: 'error',
            message: apiErrorMessage(deviceResult.error),
          })
        } else {
          setDevices({ status: 'ready', value: deviceResult.data.data.devices })
        }
      } catch (cause) {
        if (cancelled) {
          return
        }
        const message = cause instanceof Error ? cause.message : String(cause)
        setProfile({ status: 'error', message })
        setDevices({ status: 'error', message })
      }
    })()
    return () => {
      cancelled = true
    }
  }, [api, accountId, userId, reconnects])

  const roomMember = members.members.value.get(userId)
  const readyProfile = profile.status === 'ready' ? profile.value : null
  const shownMember = headerMember(userId, roomMember, readyProfile)
  const headerName =
    roomMember !== undefined
      ? memberDisplay(roomMember)
      : (blankToNull(readyProfile?.display_name) ?? userId)
  const headerMxc = blankToNull(shownMember?.avatar_url)
  const self = ownUserId !== null && ownUserId === userId
  const roomName = blankToNull(roomMember?.display_name)
  const roomAvatar = blankToNull(roomMember?.avatar_url)
  const globalName = blankToNull(readyProfile?.display_name)
  const globalAvatar = blankToNull(readyProfile?.avatar_url)

  const startDm = async () => {
    if (startingDm) {
      return
    }
    setDmError(null)
    setStartingDm(true)
    const result = await rooms.createDm(accountId, userId)
    setStartingDm(false)
    if (!result.ok) {
      setDmError(`Could not start DM with ${userId}: ${result.message}`)
      return
    }
    location.route(localRoomHref(accountId, result.roomId, null))
    onClose()
  }

  return (
    <aside
      id="user-info-panel"
      class="side-panel room-info-panel user-info-panel"
      aria-label="User information"
    >
      <div class="overlay-head">
        <h2>User information</h2>
        <button type="button" class="ghost" onClick={onClose}>
          Close
        </button>
      </div>

      <div class="room-info-identity">
        {headerMxc === null ? (
          <UserAvatar
            accountId={accountId}
            userId={userId}
            displayName={headerName}
            member={shownMember}
          />
        ) : (
          <button
            type="button"
            class="room-info-avatar-button"
            aria-label={`View the ${headerName} avatar at full size`}
            onClick={() => setAvatarOpen(true)}
          >
            <UserAvatar
              accountId={accountId}
              userId={userId}
              displayName={headerName}
              member={shownMember}
            />
          </button>
        )}
        <div class="room-info-identity-copy">
          <p class="room-info-identity-name">{headerName}</p>
        </div>
      </div>

      <section class="room-info-section" aria-labelledby="user-info-details">
        <h3 id="user-info-details">Details</h3>
        <dl class="detail-list">
          <DetailRow label="Matrix ID" value={userId} code />
          <DetailRow
            label="Membership"
            value={
              members.loading.value && roomMember === undefined
                ? 'Loading…'
                : roomMember === undefined
                  ? 'Not in this room'
                  : membershipLabel(roomMember.membership)
            }
          />
          {roomMember !== undefined && (
            <DetailRow label="Room display name" value={roomName ?? 'None'} />
          )}
          {roomMember !== undefined && (
            <DetailRow label="Room avatar" value={roomAvatar ?? 'None'} code />
          )}
          {profile.status === 'loading' && (
            <DetailRow label="Global profile" value="Loading…" />
          )}
          {roomMember === undefined && profile.status === 'ready' && (
            <DetailRow label="Display name" value={globalName ?? 'None'} />
          )}
          {roomMember === undefined && profile.status === 'ready' && (
            <DetailRow label="Avatar" value={globalAvatar ?? 'None'} code />
          )}
          {roomMember !== undefined &&
            profile.status === 'ready' &&
            globalName !== roomName && (
              <DetailRow
                label="Global display name"
                value={globalName ?? 'None'}
              />
            )}
          {roomMember !== undefined &&
            profile.status === 'ready' &&
            globalAvatar !== roomAvatar && (
              <DetailRow
                label="Global avatar"
                value={globalAvatar ?? 'None'}
                code
              />
            )}
        </dl>
        {profile.status === 'error' && (
          <p class="error" role="alert">
            {profile.message}
          </p>
        )}
      </section>

      <section class="room-info-section" aria-labelledby="user-info-devices">
        <h3 id="user-info-devices">Devices</h3>
        {devices.status === 'error' ? (
          <p class="error" role="alert">
            {devices.message}
          </p>
        ) : devices.status === 'loading' ? (
          <p class="muted">Loading devices…</p>
        ) : devices.value.length === 0 ? (
          <p class="muted">Axon has not tracked any devices for this user.</p>
        ) : (
          <ul class="user-device-list">
            {devices.value.map((device) => {
              const name = blankToNull(device.display_name) ?? 'Unnamed device'
              const algorithms =
                device.algorithms.length === 0
                  ? 'None'
                  : device.algorithms.join(', ')
              return (
                <li class="user-device" key={device.device_id}>
                  <span class="user-device-name">{name}</span>
                  <code>{device.device_id}</code>
                  <span class="muted">
                    {device.is_verified ? 'Verified' : 'Not verified'};{' '}
                    {device.is_cross_signed_by_owner
                      ? 'Cross-signed by owner'
                      : 'Not cross-signed by owner'}
                  </span>
                  <span class="muted">
                    Local trust: {localTrustLabel(device.local_trust_state)}
                  </span>
                  <span class="muted">Algorithms: {algorithms}</span>
                </li>
              )
            })}
          </ul>
        )}
      </section>

      <section class="room-info-section" aria-labelledby="user-info-actions">
        <h3 id="user-info-actions">Actions</h3>
        <div class="room-info-actions">
          {self ? (
            <button
              type="button"
              disabled
              title="Editing your display name is not available in the web client yet"
            >
              Edit display name
            </button>
          ) : (
            <>
              <button
                type="button"
                disabled={startingDm}
                onClick={() => void startDm()}
              >
                {startingDm ? 'Starting…' : 'Direct message'}
              </button>
              <button
                type="button"
                disabled
                title="Ignoring a user is not available in the web client yet"
              >
                Ignore
              </button>
              <button
                type="button"
                disabled
                title="Starting verification with this user is not available in the web client yet"
              >
                Verify
              </button>
            </>
          )}
        </div>
        {dmError !== null && (
          <p class="error" role="alert">
            {dmError}
          </p>
        )}
      </section>

      {avatarOpen && headerMxc !== null && (
        <Lightbox
          label={`${headerName} avatar`}
          caption={null}
          onClose={() => setAvatarOpen(false)}
        >
          <LightboxImage
            accountId={accountId}
            media={{
              kind: 'image',
              url: headerMxc,
              thumbnailUrl: null,
              filename: `${headerName} avatar`,
              caption: null,
              encrypted: false,
            }}
          />
        </Lightbox>
      )}
    </aside>
  )
}
