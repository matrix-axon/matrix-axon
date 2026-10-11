import { useLocation } from 'preact-iso'
import { useEffect, useRef, useState } from 'preact/hooks'
import { apiErrorMessage } from '../api/client'
import type { components } from '../api/schema'
import { localRoomHref } from '../matrix-to'
import { useServices } from '../services'
import type { MembersStore } from '../stores/members'
import {
  memberDisplay,
  membershipLabel,
  type MemberDto,
} from '../stores/room-list'
import { useShortcuts } from '../shortcuts'
import { BodyPortal } from './BodyPortal'
import { DetailRow } from './DetailRow'
import { Lightbox, LightboxImage } from './Lightbox'
import { useModalFocus } from './use-modal-focus'
import { useStartDm } from './use-start-dm'
import { UserAvatar } from './UserAvatar'

type MatrixProfileDto = components['schemas']['MatrixProfileDto']
type DeviceDto = components['schemas']['DeviceDto']

type Remote<T> =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'ready'; value: T; refreshError: string | null }

/** A failed refresh keeps data that is already on screen. */
function keepShown<T>(current: Remote<T>, message: string): Remote<T> {
  if (current.status === 'ready') {
    return { status: 'ready', value: current.value, refreshError: message }
  }
  return { status: 'error', message }
}

function blankToNull(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? ''
  return trimmed === '' ? null : trimmed
}

/**
 * An action the web client does not offer yet.
 *
 * It stays a real button, drawn quiet, because a `title` tooltip is not shown
 * on touch and a disabled button does not receive the tap. Hover still shows
 * "coming soon". A tap opens the same notice.
 */
function UnavailableAction({ label }: { label: string }) {
  const [open, setOpen] = useState(false)
  return (
    <>
      <button
        type="button"
        class="quiet"
        title="coming soon"
        aria-haspopup="dialog"
        onClick={() => setOpen(true)}
      >
        {label}
      </button>
      {open && <ComingSoonNotice onClose={() => setOpen(false)} />}
    </>
  )
}

function ComingSoonNotice({ onClose }: { onClose: () => void }) {
  const { containerRef } = useModalFocus<HTMLDivElement>()
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
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
        aria-labelledby="user-info-coming-soon"
        onClick={(event) => {
          if (event.target === event.currentTarget) {
            onClose()
          }
        }}
      >
        <div class="overlay-panel coming-soon-dialog">
          <h2 id="user-info-coming-soon">Coming soon</h2>
          <div class="dialog-actions">
            <button type="button" onClick={onClose}>
              Close
            </button>
          </div>
        </div>
      </div>
    </BodyPortal>
  )
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
  const { api, live } = useServices()
  const reconnects = live.reconnects.value
  const closeButton = useRef<HTMLButtonElement>(null)
  const [profile, setProfile] = useState<Remote<MatrixProfileDto>>({
    status: 'loading',
  })
  const [devices, setDevices] = useState<Remote<readonly DeviceDto[]>>({
    status: 'loading',
  })
  const [avatarOpen, setAvatarOpen] = useState(false)
  const {
    startDm,
    pendingUserId,
    error: dmError,
    invalidate,
  } = useStartDm(accountId, (roomId) => {
    location.route(localRoomHref(accountId, roomId, null))
    onClose()
  })

  // The close control is the landing spot. The name button keeps the visible
  // name as its accessible name, so opening the panel has to move focus here
  // or the keyboard stays on the timeline (and on a covered thread, on a
  // control that is no longer reachable).
  useEffect(() => {
    closeButton.current?.focus()
  }, [userId])

  // A new subject discards what the previous one showed. A reconnect does not:
  // the reads below refresh in place, and an open avatar stays open.
  useEffect(() => {
    setProfile({ status: 'loading' })
    setDevices({ status: 'loading' })
    setAvatarOpen(false)
    invalidate()
  }, [accountId, userId, invalidate])

  // Membership and the room profile come from the room's members store, which
  // the room page refreshes on open and on reconnect. The global profile is a
  // homeserver read. The device list is the local crypto cache and must not
  // wait on that read. An empty device list means Axon has never tracked that
  // user. Each read retries when the socket reconnects without blanking a
  // value that is already on screen.
  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const result = await api.GET(
          '/v1/accounts/{account_id}/users/{user_id}/profile',
          {
            params: { path: { account_id: accountId, user_id: userId } },
          },
        )
        if (cancelled) {
          return
        }
        if (result.error !== undefined || result.data === undefined) {
          const message = apiErrorMessage(result.error)
          setProfile((current) => keepShown(current, message))
          return
        }
        setProfile({
          status: 'ready',
          value: result.data.data,
          refreshError: null,
        })
      } catch (cause) {
        if (cancelled) {
          return
        }
        const message = cause instanceof Error ? cause.message : String(cause)
        setProfile((current) => keepShown(current, message))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [api, accountId, userId, reconnects])

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const result = await api.GET('/v1/accounts/{account_id}/devices', {
          params: {
            path: { account_id: accountId },
            query: { user_id: userId },
          },
        })
        if (cancelled) {
          return
        }
        if (result.error !== undefined || result.data === undefined) {
          const message = apiErrorMessage(result.error)
          setDevices((current) => keepShown(current, message))
          return
        }
        setDevices({
          status: 'ready',
          value: result.data.data.devices,
          refreshError: null,
        })
      } catch (cause) {
        if (cancelled) {
          return
        }
        const message = cause instanceof Error ? cause.message : String(cause)
        setDevices((current) => keepShown(current, message))
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
  const startingDm = pendingUserId !== null

  return (
    <aside
      id="user-info-panel"
      class="side-panel room-info-panel user-info-panel"
      aria-label="User information"
    >
      <div class="overlay-head">
        <h2>User information</h2>
        <button ref={closeButton} type="button" class="ghost" onClick={onClose}>
          Close
        </button>
      </div>

      <div class="user-info-intro">
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

        <div class="room-info-actions">
          {self ? (
            <UnavailableAction label="Edit display name" />
          ) : (
            <>
              <button
                type="button"
                disabled={startingDm}
                onClick={() => void startDm(userId)}
              >
                {startingDm ? 'Starting…' : 'Direct message'}
              </button>
              <UnavailableAction label="Ignore" />
              <UnavailableAction label="Verify" />
            </>
          )}
        </div>
        {dmError !== null && (
          <p class="error" role="alert">
            {dmError}
          </p>
        )}
      </div>

      <section class="room-info-section" aria-labelledby="user-info-details">
        <h3 id="user-info-details">Details</h3>
        <dl class="detail-list">
          <DetailRow label="Matrix ID" value={userId} code copyable />
          <DetailRow
            label="Membership"
            copyable={false}
            value={
              members.loading.value && roomMember === undefined
                ? 'Loading…'
                : roomMember === undefined
                  ? 'Not in this room'
                  : membershipLabel(roomMember.membership)
            }
          />
          {roomMember !== undefined && (
            <DetailRow
              label="Room display name"
              value={roomName ?? 'None'}
              copyable={roomName !== null}
            />
          )}
          {roomMember !== undefined && (
            <DetailRow
              label="Room avatar"
              value={roomAvatar ?? 'None'}
              code
              copyable={roomAvatar !== null}
            />
          )}
          {profile.status === 'loading' && (
            <DetailRow
              label="Global profile"
              value="Loading…"
              copyable={false}
            />
          )}
          {roomMember === undefined && profile.status === 'ready' && (
            <DetailRow
              label="Display name"
              value={globalName ?? 'None'}
              copyable={globalName !== null}
            />
          )}
          {roomMember === undefined && profile.status === 'ready' && (
            <DetailRow
              label="Avatar"
              value={globalAvatar ?? 'None'}
              code
              copyable={globalAvatar !== null}
            />
          )}
          {roomMember !== undefined &&
            profile.status === 'ready' &&
            globalName !== roomName && (
              <DetailRow
                label="Global display name"
                value={globalName ?? 'None'}
                copyable={globalName !== null}
              />
            )}
          {roomMember !== undefined &&
            profile.status === 'ready' &&
            globalAvatar !== roomAvatar && (
              <DetailRow
                label="Global avatar"
                value={globalAvatar ?? 'None'}
                code
                copyable={globalAvatar !== null}
              />
            )}
        </dl>
        {profile.status === 'error' && (
          <p class="error" role="alert">
            {profile.message}
          </p>
        )}
        {profile.status === 'ready' && profile.refreshError !== null && (
          <p class="error" role="alert">
            {profile.refreshError}
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
        {devices.status === 'ready' && devices.refreshError !== null && (
          <p class="error" role="alert">
            {devices.refreshError}
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
