import { useEffect, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { ErrorBanner } from '../components/ErrorBanner'
import { RoomAvatar, roomAvatarColor } from '../components/RoomAvatar'
import { useServices } from '../services'
import {
  inviteKey,
  type InviteBulkResult,
  type InviteDto,
} from '../stores/invites'

function inviteTitle(invite: InviteDto): string {
  const name = invite.name?.trim()
  if (name !== undefined && name !== '') {
    return name
  }
  const alias = invite.canonical_alias?.trim()
  if (alias !== undefined && alias !== '') {
    return alias
  }
  return invite.room_id
}

function inviterLabel(invite: InviteDto): string {
  const name = invite.inviter_display_name?.trim()
  if (name !== undefined && name !== '') {
    return `${name} (${invite.inviter_user_id})`
  }
  return invite.inviter_user_id
}

/**
 * Pending-invite inbox (ADR 0091). Mounted at `/invites`; accept/reject
 * reuse join/leave. The sidebar Invites row is the only way in.
 */
export function InvitesPage() {
  const { invites, accounts } = useServices()
  const location = useLocation()
  const [busyKey, setBusyKey] = useState<string | null>(null)
  const [bulkBusy, setBulkBusy] = useState<'accept' | 'reject' | null>(null)
  const [bulkResult, setBulkResult] = useState<InviteBulkResult | null>(null)
  const showAccount =
    accounts.accounts.value.filter((account) => account.state === 'active')
      .length > 1

  useEffect(() => {
    void invites.ensureLoaded()
  }, [invites])

  const rows = invites.invites.value

  async function onAccept(invite: InviteDto) {
    invites.error.value = null
    // A bulk run's outcome describes rows that are no longer on screen. Left
    // up, its "N failed" would keep claiming a failure the user has since
    // cleared one row at a time.
    setBulkResult(null)
    setBusyKey(inviteKey(invite))
    const result = await invites.accept(invite)
    setBusyKey(null)
    if (!result.ok) {
      invites.error.value = result.message
      return
    }
    location.route(
      `/${invite.account_id}/rooms/${encodeURIComponent(invite.room_id)}`,
    )
  }

  async function onReject(invite: InviteDto) {
    invites.error.value = null
    setBulkResult(null)
    setBusyKey(inviteKey(invite))
    const result = await invites.reject(invite)
    setBusyKey(null)
    if (!result.ok) {
      invites.error.value = result.message
    }
  }

  async function onBulk(kind: 'accept' | 'reject') {
    invites.error.value = null
    setBulkResult(null)
    setBulkBusy(kind)
    // `runBulk` already refreshes the room list after a successful accept —
    // repeating it here issued a second GET /v1/rooms per bulk accept.
    const result =
      kind === 'accept' ? await invites.acceptAll() : await invites.rejectAll()
    setBulkBusy(null)
    setBulkResult(result)
  }

  return (
    <section class="invites-page">
      <header class="invites-header">
        <div>
          <h1>Invites</h1>
          <p class="muted">
            {invites.count.value === 0
              ? 'No pending invites'
              : `${invites.count.value} pending`}
          </p>
        </div>
        {rows.length > 0 && (
          <div class="invites-bulk">
            <button
              type="button"
              disabled={bulkBusy !== null}
              onClick={() => void onBulk('accept')}
            >
              {bulkBusy === 'accept' ? 'Accepting…' : 'Accept all'}
            </button>
            <button
              type="button"
              class="danger"
              disabled={bulkBusy !== null}
              onClick={() => void onBulk('reject')}
            >
              {bulkBusy === 'reject' ? 'Rejecting…' : 'Reject all'}
            </button>
          </div>
        )}
      </header>
      <ErrorBanner error={invites.error} />
      {bulkResult !== null && bulkResult.failed.length > 0 && (
        <p class="error" role="alert">
          {bulkResult.succeeded} succeeded, {bulkResult.failed.length} failed.
          {bulkResult.failed
            .map((item) => ` ${inviteTitle(item.invite)}: ${item.message}`)
            .join('')}
        </p>
      )}
      {invites.loading.value && rows.length === 0 ? (
        <p>Loading invites…</p>
      ) : rows.length === 0 ? (
        <p>No pending invites.</p>
      ) : (
        <ul class="invite-list">
          {rows.map((invite) => (
            <InviteRow
              key={inviteKey(invite)}
              invite={invite}
              showAccount={showAccount}
              busy={busyKey === inviteKey(invite) || bulkBusy !== null}
              onAccept={() => void onAccept(invite)}
              onReject={() => void onReject(invite)}
            />
          ))}
        </ul>
      )}
    </section>
  )
}

function InviteRow({
  invite,
  showAccount,
  busy,
  onAccept,
  onReject,
}: {
  invite: InviteDto
  showAccount: boolean
  busy: boolean
  onAccept: () => void
  onReject: () => void
}) {
  const title = inviteTitle(invite)
  return (
    <li class="invite-row">
      <RoomAvatar
        accountId={invite.account_id}
        mxcUrl={invite.avatar_url ?? null}
        title={title}
        color={roomAvatarColor(inviteKey(invite))}
      />
      <div class="invite-body">
        <p class="invite-name">{title}</p>
        <p class="muted">
          Invited by {inviterLabel(invite)}
          {invite.is_direct ? ' · Direct message' : ''}
          {invite.encrypted ? ' · Encrypted' : ''}
          {showAccount ? ` · ${invite.account_user_id}` : ''}
        </p>
      </div>
      <div class="invite-actions">
        <button type="button" disabled={busy} onClick={onAccept}>
          Accept
        </button>
        <button type="button" class="danger" disabled={busy} onClick={onReject}>
          Reject
        </button>
      </div>
    </li>
  )
}
