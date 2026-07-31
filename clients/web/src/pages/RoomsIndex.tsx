import { useLocation } from 'preact-iso'
import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { apiErrorCode, apiErrorMessage } from '../api/client'
import type { components } from '../api/schema'
import {
  localRoomHref,
  parseMatrixRoomReference,
  serverNameFromRoomReference,
} from '../matrix-to'
import { normalizeUserId, parseUserIdList } from '../matrix-user'
import { joinRoomError } from '../room-entry'
import { useServices } from '../services'
import type { Account } from '../stores/accounts'
import type { RoomDto } from '../stores/room-list'
import type { RoomEntryResult } from '../stores/rooms'

type PublicRoomSummary = components['schemas']['PublicRoomSummaryDto']
type MatrixProfile = components['schemas']['MatrixProfileDto']
type DirectoryPageRequest = {
  since: string | null
  server: string | null
  searchTerm: string
}
type RoomActionTarget = 'join' | 'dm' | 'create' | 'find'

const DIRECTORY_LIMIT = 25
const DIRECTORY_SERVER_RECENTS_KEY = 'axon.publicRoomDirectoryServers'
const DIRECTORY_SERVER_SUGGESTIONS = ['matrix.org', 'matrixrooms.info']

/**
 * The `/` route's right pane (ADR 0062). The room list itself lives in the
 * shell sidebar, so this page owns discovery: direct room entry plus public
 * room directory search (M19-W3).
 */
export function RoomsIndex() {
  const { accounts, api, rooms, settings } = useServices()
  const location = useLocation()
  const joinInput = useRef<HTMLInputElement>(null)
  const joinButton = useRef<HTMLButtonElement>(null)
  const createNameInput = useRef<HTMLInputElement>(null)
  const dmUserInput = useRef<HTMLInputElement>(null)
  const directorySearchInput = useRef<HTMLInputElement>(null)
  const activeAccounts = accounts.accounts.value.filter(
    (account) => account.state === 'active',
  )
  const [selectedAccountId, setSelectedAccountId] = useState<string | null>(
    null,
  )
  const selectedAccount =
    activeAccounts.find(
      (account) => account.account_id === selectedAccountId,
    ) ??
    activeAccounts.find(
      (account) => account.account_id === settings.activeAccountId.value,
    ) ??
    activeAccounts[0] ??
    null
  const accountId = selectedAccount?.account_id ?? null
  const homeServerName =
    selectedAccount === null ? null : serverNameFromAccount(selectedAccount)
  const previousAccountId = useRef<string | null>(null)
  const [joinTarget, setJoinTarget] = useState('')
  const [joinStatus, setJoinStatus] = useState<string | null>(null)
  const [joining, setJoining] = useState(false)
  const [roomName, setRoomName] = useState('')
  const [roomTopic, setRoomTopic] = useState('')
  const [roomInvite, setRoomInvite] = useState('')
  const [roomPublic, setRoomPublic] = useState(false)
  const [roomEncrypted, setRoomEncrypted] = useState(true)
  const [createStatus, setCreateStatus] = useState<string | null>(null)
  const [creatingRoom, setCreatingRoom] = useState(false)
  const [dmUser, setDmUser] = useState('')
  const [dmStatus, setDmStatus] = useState<string | null>(null)
  const [dmProfile, setDmProfile] = useState<MatrixProfile | null>(null)
  const [profileLoading, setProfileLoading] = useState(false)
  const [creatingDm, setCreatingDm] = useState(false)
  const [searchTerm, setSearchTerm] = useState('')
  const [directoryServer, setDirectoryServer] = useState('')
  const [directoryRecents, setDirectoryRecents] = useState<string[]>(() =>
    loadRecentDirectoryServers(),
  )
  const [directoryResults, setDirectoryResults] = useState<PublicRoomSummary[]>(
    [],
  )
  const [directoryError, setDirectoryError] = useState<string | null>(null)
  const [directoryLoading, setDirectoryLoading] = useState(false)
  const [directorySearched, setDirectorySearched] = useState(false)
  const [nextBatch, setNextBatch] = useState<DirectoryPageRequest | null>(null)
  const [totalEstimate, setTotalEstimate] = useState<number | null>(null)
  const [joiningRoomId, setJoiningRoomId] = useState<string | null>(null)

  const clearDirectoryResults = () => {
    setDirectoryResults([])
    setNextBatch(null)
    setTotalEstimate(null)
  }

  const directoryServerOptions = useMemo(
    () =>
      [...new Set([...DIRECTORY_SERVER_SUGGESTIONS, ...directoryRecents])].sort(
        (a, b) => a.localeCompare(b),
      ),
    [directoryRecents],
  )

  useEffect(() => {
    if (accounts.loading.value) {
      void accounts.refresh()
    }
    void rooms.ensureLoaded()
  }, [accounts, rooms])
  useEffect(() => {
    switch (roomActionTarget(location.path, window.location.hash)) {
      case 'join':
        joinInput.current?.focus()
        break
      case 'dm':
        dmUserInput.current?.focus()
        break
      case 'create':
        createNameInput.current?.focus()
        break
      case 'find':
        directorySearchInput.current?.focus()
        break
    }
  }, [location.path, location.url])
  useEffect(() => {
    if (location.path !== '/rooms/dm') {
      return
    }
    const user = new URLSearchParams(window.location.search).get('user')
    if (user === null || user.trim() === '') {
      return
    }
    setDmUser(user)
    setDmProfile(null)
    setDmStatus(null)
  }, [location.path, location.url])
  useEffect(() => {
    if (previousAccountId.current === accountId) {
      return
    }
    const previous = previousAccountId.current
    previousAccountId.current = accountId
    if (previous === null) {
      return
    }
    setJoinTarget('')
    setJoinStatus(null)
    setJoining(false)
    setRoomName('')
    setRoomTopic('')
    setRoomInvite('')
    setRoomPublic(false)
    setRoomEncrypted(true)
    setCreateStatus(null)
    setCreatingRoom(false)
    setDmUser('')
    setDmStatus(null)
    setDmProfile(null)
    setProfileLoading(false)
    setCreatingDm(false)
    setDirectoryError(null)
    setDirectorySearched(false)
    setJoiningRoomId(null)
    clearDirectoryResults()
  }, [accountId])

  const joinReference = async (
    target: string,
    statusLabel: string,
  ): Promise<void> => {
    setJoinStatus(null)
    if (accountId === null) {
      setJoinStatus('Add an active account before joining rooms.')
      return
    }
    const reference = parseMatrixRoomReference(target, {
      allowAliasShorthand: true,
      defaultAliasServerName: homeServerName,
    })
    if (reference === null) {
      setJoinStatus('Enter a room id, alias, Matrix.to link, or matrix: URI.')
      return
    }
    const existingRoomId = joinedRoomIdForReference(
      accountId,
      reference.roomIdOrAlias,
      rooms.rooms.value,
    )
    if (existingRoomId !== null) {
      location.route(
        localRoomHref(accountId, existingRoomId, reference.eventId),
      )
      return
    }
    setJoining(true)
    const result = await rooms.joinRoom(
      accountId,
      reference.roomIdOrAlias,
      reference.serverNames,
    )
    setJoining(false)
    if (!result.ok) {
      setJoinStatus(joinRoomError(statusLabel, result))
      return
    }
    setJoinTarget('')
    location.route(localRoomHref(accountId, result.roomId, reference.eventId))
  }

  const lookupDmProfile = async () => {
    setDmStatus(null)
    setDmProfile(null)
    if (accountId === null) {
      setDmStatus('Add an active account before starting DMs.')
      return
    }
    const userId = normalizeUserId(dmUser, homeServerName)
    if (userId === null) {
      setDmStatus('Enter a Matrix user ID like @alice:example.org.')
      return
    }
    if (userId === selectedAccount?.user_id) {
      setDmStatus('Select another user to start a direct message.')
      return
    }
    setProfileLoading(true)
    try {
      const { data, error: apiError } = await api.GET(
        '/v1/accounts/{account_id}/users/{user_id}/profile',
        {
          params: {
            path: { account_id: accountId, user_id: userId },
          },
        },
      )
      if (apiError !== undefined || data === undefined) {
        setDmStatus(
          apiError === undefined
            ? 'Profile lookup failed.'
            : dmProfileLookupError(userId, apiError),
        )
        return
      }
      setDmUser(userId)
      setDmProfile(data.data)
    } catch (cause) {
      setDmStatus(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setProfileLoading(false)
    }
  }

  const createDm = async () => {
    setDmStatus(null)
    if (accountId === null) {
      setDmStatus('Add an active account before starting DMs.')
      return
    }
    const userId = normalizeUserId(dmUser, homeServerName)
    if (userId === null) {
      setDmStatus('Enter a Matrix user ID like @alice:example.org.')
      return
    }
    if (userId === selectedAccount?.user_id) {
      setDmStatus('Select another user to start a direct message.')
      return
    }
    setCreatingDm(true)
    const result = await rooms.createDm(accountId, userId)
    setCreatingDm(false)
    if (!result.ok) {
      setDmStatus(`Could not start DM with ${userId}: ${result.message}`)
      return
    }
    setDmUser('')
    setDmProfile(null)
    location.route(localRoomHref(accountId, result.roomId, null))
  }

  const createRoom = async () => {
    setCreateStatus(null)
    if (accountId === null) {
      setCreateStatus('Add an active account before creating rooms.')
      return
    }
    const invite = parseUserIdList(roomInvite, homeServerName)
    if (!invite.ok) {
      setCreateStatus(invite.message)
      return
    }
    setCreatingRoom(true)
    const result = await rooms.createRoom(accountId, {
      name: emptyToNull(roomName),
      topic: emptyToNull(roomTopic),
      invite: invite.userIds,
      is_direct: false,
      public: roomPublic,
      preset: roomPublic ? 'public_chat' : 'private_chat',
      encrypted: roomEncrypted,
    })
    setCreatingRoom(false)
    if (!result.ok) {
      setCreateStatus(createRoomError(result))
      return
    }
    setRoomName('')
    setRoomTopic('')
    setRoomInvite('')
    setRoomPublic(false)
    setRoomEncrypted(true)
    location.route(localRoomHref(accountId, result.roomId, null))
  }

  const searchDirectory = async (pageRequest?: DirectoryPageRequest) => {
    setDirectoryError(null)
    const freshSearch = pageRequest === undefined || pageRequest.since === null
    if (accountId === null) {
      if (freshSearch) {
        clearDirectoryResults()
        setDirectorySearched(false)
      }
      setDirectoryError('Add an active account before searching directories.')
      return
    }
    const request =
      pageRequest ??
      ({
        since: null,
        server: normalizeDirectoryServer(directoryServer),
        searchTerm: searchTerm.trim(),
      } satisfies DirectoryPageRequest)
    if (
      pageRequest === undefined &&
      directoryServer.trim() !== '' &&
      request.server === null
    ) {
      clearDirectoryResults()
      setDirectorySearched(false)
      setDirectoryError('Enter a homeserver name, such as matrix.org.')
      return
    }
    if (freshSearch) {
      clearDirectoryResults()
    }
    setDirectoryLoading(true)
    setDirectorySearched(true)
    try {
      const { data, error: apiError } = await api.GET(
        '/v1/accounts/{account_id}/directory/public_rooms',
        {
          params: {
            path: { account_id: accountId },
            query: {
              limit: DIRECTORY_LIMIT,
              ...(request.server === null ? {} : { server: request.server }),
              ...(request.searchTerm === ''
                ? {}
                : { search_term: request.searchTerm }),
              ...(request.since === null ? {} : { since: request.since }),
            },
          },
        },
      )
      if (apiError !== undefined || data === undefined) {
        setDirectoryError(
          apiError === undefined
            ? 'Directory search failed.'
            : apiErrorMessage(apiError),
        )
        if (freshSearch) {
          setDirectorySearched(false)
        }
        return
      }
      const page = data.data
      setDirectoryResults((current) =>
        request.since === null ? page.chunk : [...current, ...page.chunk],
      )
      setNextBatch(
        page.next_batch === undefined || page.next_batch === null
          ? null
          : { ...request, since: page.next_batch },
      )
      setTotalEstimate(page.total_room_count_estimate ?? null)
      if (request.server !== null) {
        const recents = rememberDirectoryServer(request.server)
        if (recents !== null) {
          setDirectoryRecents(recents)
        }
      }
    } catch (cause) {
      setDirectoryError(cause instanceof Error ? cause.message : String(cause))
      if (freshSearch) {
        setDirectorySearched(false)
      }
    } finally {
      setDirectoryLoading(false)
    }
  }

  const joinDirectoryRoom = async (room: PublicRoomSummary) => {
    if (accountId === null) {
      setDirectoryError('Add an active account before joining rooms.')
      return
    }
    const existingRoomId = joinedRoomIdForDirectoryRoom(
      accountId,
      room,
      rooms.rooms.value,
    )
    if (existingRoomId !== null) {
      location.route(localRoomHref(accountId, existingRoomId, null))
      return
    }
    setDirectoryError(null)
    setJoiningRoomId(room.room_id)
    const canonicalAlias = room.canonical_alias ?? null
    const target = canonicalAlias ?? room.room_id
    const server = serverNameFromRoomReference(room.room_id)
    const result = await rooms.joinRoom(
      accountId,
      target,
      canonicalAlias === null && server !== null ? [server] : [],
    )
    setJoiningRoomId(null)
    if (!result.ok) {
      setDirectoryError(joinRoomError(roomLabel(room), result))
      return
    }
    location.route(localRoomHref(accountId, result.roomId, null))
  }

  return (
    <div class="page rooms-index room-discovery">
      <h1>Add a Room</h1>
      <p class="muted">
        Join by room id or Matrix link, create rooms, start direct messages, or
        search a public room directory.
      </p>

      {activeAccounts.length > 1 && (
        <label class="field-label">
          Account
          <select
            value={selectedAccount?.account_id ?? ''}
            onChange={(event) =>
              setSelectedAccountId(event.currentTarget.value)
            }
          >
            {activeAccounts.map((account) => (
              <option key={account.account_id} value={account.account_id}>
                {account.user_id}
              </option>
            ))}
          </select>
        </label>
      )}

      <section id="join" class="panel room-discovery-panel">
        <h2>Join Directly</h2>
        <form
          class="inline-form room-entry-form"
          onSubmit={(event) => {
            event.preventDefault()
            const target = joinInput.current?.value ?? joinTarget
            void joinReference(target, target)
          }}
        >
          <label>
            Room
            <input
              ref={joinInput}
              type="text"
              value={joinTarget}
              placeholder="#room:server, !room:server, matrix:room/..."
              autocapitalize="none"
              autocorrect="off"
              spellcheck={false}
              inputmode="url"
              onInput={(event) => setJoinTarget(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (
                  event.key === 'Tab' &&
                  !event.shiftKey &&
                  !event.ctrlKey &&
                  !event.metaKey &&
                  !event.altKey &&
                  event.currentTarget.value.trim() === ''
                ) {
                  event.preventDefault()
                  directorySearchInput.current?.focus()
                } else if (
                  event.key === 'Tab' &&
                  !event.shiftKey &&
                  !event.ctrlKey &&
                  !event.metaKey &&
                  !event.altKey
                ) {
                  event.preventDefault()
                  joinButton.current?.focus()
                }
              }}
            />
          </label>
          <button
            ref={joinButton}
            type="submit"
            disabled={joining || accountId === null}
          >
            {joining ? 'Joining...' : 'Join'}
          </button>
        </form>
        {homeServerName !== null && (
          <p class="muted">
            Short aliases like <code>#room</code> use{' '}
            <code>{homeServerName}</code>.
          </p>
        )}
        {joinStatus !== null && <p class="error">{joinStatus}</p>}
      </section>

      <section id="dm" class="panel room-discovery-panel">
        <h2>Start Direct Message</h2>
        <form
          class="room-dm-form"
          onSubmit={(event) => {
            event.preventDefault()
            void createDm()
          }}
        >
          <label>
            User
            <input
              ref={dmUserInput}
              type="text"
              value={dmUser}
              placeholder="@alice:example.org"
              autocapitalize="none"
              autocorrect="off"
              spellcheck={false}
              inputmode="email"
              onInput={(event) => {
                setDmUser(event.currentTarget.value)
                setDmProfile(null)
                setDmStatus(null)
              }}
            />
          </label>
          <button
            type="button"
            class="ghost"
            disabled={profileLoading || creatingDm || accountId === null}
            onClick={() => void lookupDmProfile()}
          >
            {profileLoading ? 'Looking up...' : 'Look up profile'}
          </button>
          <button type="submit" disabled={creatingDm || accountId === null}>
            {creatingDm ? 'Starting...' : 'Start DM'}
          </button>
        </form>
        <p class="muted">
          Localparts use the selected account homeserver. Homeserver-wide user
          search is not exposed by Axon yet. New DMs invite the other person;
          they may need to accept before they can read messages.
        </p>
        {dmProfile !== null && (
          <p class="muted">
            Profile: {dmProfile.display_name ?? dmProfile.user_id}
            {dmProfile.display_name !== null &&
              dmProfile.display_name !== undefined && (
                <>
                  {' '}
                  <code>{dmProfile.user_id}</code>
                </>
              )}
          </p>
        )}
        {dmStatus !== null && <p class="error">{dmStatus}</p>}
      </section>

      <section id="create" class="panel room-discovery-panel">
        <h2>Create Room</h2>
        <form
          class="room-create-form"
          onSubmit={(event) => {
            event.preventDefault()
            void createRoom()
          }}
        >
          <label>
            Name
            <input
              ref={createNameInput}
              type="text"
              value={roomName}
              placeholder="Room name"
              onInput={(event) => setRoomName(event.currentTarget.value)}
            />
          </label>
          <label>
            Topic
            <input
              type="text"
              value={roomTopic}
              placeholder="Optional topic"
              onInput={(event) => setRoomTopic(event.currentTarget.value)}
            />
          </label>
          <label>
            Invite
            <input
              type="text"
              value={roomInvite}
              placeholder="@alice:example.org, @bob:example.org"
              autocapitalize="none"
              autocorrect="off"
              spellcheck={false}
              inputmode="email"
              onInput={(event) => setRoomInvite(event.currentTarget.value)}
            />
          </label>
          <label>
            Visibility
            <select
              value={roomPublic ? 'public' : 'private'}
              onChange={(event) =>
                setRoomPublic(event.currentTarget.value === 'public')
              }
            >
              <option value="private">Private</option>
              <option value="public">Public</option>
            </select>
          </label>
          <label class="checkbox-label">
            <input
              type="checkbox"
              checked={roomEncrypted}
              onChange={(event) =>
                setRoomEncrypted(event.currentTarget.checked)
              }
            />
            Encrypt from creation
          </label>
          <button type="submit" disabled={creatingRoom || accountId === null}>
            {creatingRoom ? 'Creating...' : 'Create room'}
          </button>
        </form>
        <p class="muted">
          Name is display-only, not a joinable alias. Axon room creation does
          not support defining aliases yet. Invite localparts use the selected
          account homeserver.
        </p>
        {createStatus !== null && <p class="error">{createStatus}</p>}
      </section>

      <section id="find" class="panel room-discovery-panel">
        <h2>Find Rooms</h2>
        <form
          class="room-directory-form"
          onSubmit={(event) => {
            event.preventDefault()
            void searchDirectory()
          }}
        >
          <label>
            Search
            <input
              ref={directorySearchInput}
              type="search"
              value={searchTerm}
              placeholder="Room name, topic, or alias"
              onInput={(event) => setSearchTerm(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (
                  event.key === 'Tab' &&
                  event.shiftKey &&
                  !event.ctrlKey &&
                  !event.metaKey &&
                  !event.altKey
                ) {
                  event.preventDefault()
                  if (joinInput.current?.value.trim() === '') {
                    joinInput.current.focus()
                  } else {
                    joinButton.current?.focus()
                  }
                }
              }}
            />
          </label>
          <label>
            Directory server
            <input
              type="text"
              list="directory-server-options"
              value={directoryServer}
              placeholder={
                homeServerName === null
                  ? 'Account homeserver'
                  : `Account homeserver (${homeServerName})`
              }
              onInput={(event) => setDirectoryServer(event.currentTarget.value)}
            />
          </label>
          <datalist id="directory-server-options">
            {directoryServerOptions.map((server) => (
              <option key={server} value={server} />
            ))}
          </datalist>
          <button
            type="submit"
            disabled={directoryLoading || accountId === null}
          >
            {directoryLoading ? 'Searching...' : 'Search directory'}
          </button>
        </form>
        <p class="muted">
          Leave the server blank for your account homeserver. Try{' '}
          <button
            type="button"
            class="link-button"
            onClick={() => setDirectoryServer('matrix.org')}
          >
            matrix.org
          </button>{' '}
          or{' '}
          <button
            type="button"
            class="link-button"
            onClick={() => setDirectoryServer('matrixrooms.info')}
          >
            matrixrooms.info
          </button>
          ; recent successful directory servers are remembered in this browser.
        </p>
        {directoryError !== null && <p class="error">{directoryError}</p>}
        <DirectoryResults
          accountId={accountId}
          rooms={rooms.rooms.value}
          results={directoryResults}
          searched={directorySearched}
          loading={directoryLoading}
          totalEstimate={totalEstimate}
          joiningRoomId={joiningRoomId}
          onOpen={(roomId) => {
            if (accountId !== null) {
              location.route(localRoomHref(accountId, roomId, null))
            }
          }}
          onJoin={joinDirectoryRoom}
        />
        {nextBatch !== null && (
          <button
            type="button"
            disabled={directoryLoading}
            onClick={() => void searchDirectory(nextBatch)}
          >
            {directoryLoading ? 'Loading...' : 'Load more'}
          </button>
        )}
      </section>
    </div>
  )
}

function DirectoryResults({
  accountId,
  rooms,
  results,
  searched,
  loading,
  totalEstimate,
  joiningRoomId,
  onOpen,
  onJoin,
}: {
  accountId: string | null
  rooms: readonly RoomDto[]
  results: readonly PublicRoomSummary[]
  searched: boolean
  loading: boolean
  totalEstimate: number | null
  joiningRoomId: string | null
  onOpen: (roomId: string) => void
  onJoin: (room: PublicRoomSummary) => Promise<void>
}) {
  if (!searched) {
    return null
  }
  if (loading && results.length === 0) {
    return <p>Searching directory...</p>
  }
  if (results.length === 0) {
    return <p class="muted">No public rooms found.</p>
  }
  return (
    <>
      {totalEstimate !== null && (
        <p class="muted">{totalEstimate.toLocaleString()} rooms estimated.</p>
      )}
      <ul class="cards public-room-results">
        {results.map((room) => {
          const joinedRoomId =
            accountId === null
              ? null
              : joinedRoomIdForDirectoryRoom(accountId, room, rooms)
          return (
            <li key={room.room_id} class="card public-room-card">
              <div class="card-head">
                <strong>{roomLabel(room)}</strong>
                <span class="badge">{room.join_rule}</span>
                {room.room_type !== null && room.room_type !== undefined && (
                  <span class="badge">{room.room_type}</span>
                )}
              </div>
              <div class="card-meta">
                {room.canonical_alias ?? room.room_id}
              </div>
              {room.topic !== null && room.topic !== undefined && (
                <p>{room.topic}</p>
              )}
              <p class="muted">
                {room.num_joined_members.toLocaleString()} members
                {room.world_readable ? ' · world-readable' : ''}
                {room.guest_can_join ? ' · guests can join' : ''}
              </p>
              <div class="card-actions">
                <button
                  type="button"
                  disabled={
                    accountId === null || joiningRoomId === room.room_id
                  }
                  onClick={() =>
                    joinedRoomId === null
                      ? void onJoin(room)
                      : onOpen(joinedRoomId)
                  }
                >
                  {joinedRoomId !== null
                    ? 'Open'
                    : joiningRoomId === room.room_id
                      ? 'Joining...'
                      : 'Join'}
                </button>
              </div>
            </li>
          )
        })}
      </ul>
    </>
  )
}

function serverNameFromAccount(account: Account): string | null {
  const userServer = serverNameFromRoomReference(account.user_id)
  if (userServer !== null) {
    return userServer
  }
  try {
    return new URL(account.homeserver_url).host || null
  } catch {
    return null
  }
}

function joinedRoomIdForReference(
  accountId: string,
  roomIdOrAlias: string,
  rooms: readonly RoomDto[],
): string | null {
  return (
    rooms.find(
      (room) =>
        room.account_id === accountId &&
        (room.room_id === roomIdOrAlias ||
          room.canonical_alias === roomIdOrAlias),
    )?.room_id ?? null
  )
}

function joinedRoomIdForDirectoryRoom(
  accountId: string,
  directoryRoom: PublicRoomSummary,
  rooms: readonly RoomDto[],
): string | null {
  const canonicalAlias = directoryRoom.canonical_alias ?? null
  return (
    rooms.find(
      (room) =>
        room.account_id === accountId &&
        (room.room_id === directoryRoom.room_id ||
          (canonicalAlias !== null && room.canonical_alias === canonicalAlias)),
    )?.room_id ?? null
  )
}

function roomLabel(room: PublicRoomSummary): string {
  return room.name?.trim() || room.canonical_alias || room.room_id
}

function normalizeDirectoryServer(input: string): string | null {
  const trimmed = input.trim()
  if (trimmed === '') {
    return null
  }
  if (/^https?:\/\//i.test(trimmed)) {
    try {
      return new URL(trimmed).host || null
    } catch {
      return null
    }
  }
  if (trimmed.includes('/') || trimmed.includes('@')) {
    return null
  }
  return trimmed
}

function roomActionTarget(path: string, hash: string): RoomActionTarget {
  switch (hash) {
    case '#join':
      return 'join'
    case '#dm':
      return 'dm'
    case '#create':
      return 'create'
    case '#find':
      return 'find'
  }
  switch (path) {
    case '/rooms/dm':
      return 'dm'
    case '/rooms/create':
      return 'create'
    default:
      return 'join'
  }
}

function dmProfileLookupError(userId: string, error: unknown): string {
  const message = apiErrorMessage(error)
  if (
    apiErrorCode(error) === 'not_found' ||
    /no row found.*profiles/i.test(message)
  ) {
    return `No Matrix profile was found for ${userId}. Check the user ID and homeserver, then try again.`
  }
  return `Could not look up ${userId}: ${message}`
}

function createRoomError(
  result: Extract<RoomEntryResult, { ok: false }>,
): string {
  switch (result.code) {
    case 'forbidden':
      return 'Could not create room. This homeserver does not allow this account to create rooms.'
    case 'bad_request':
      return `Could not create room. Check the room details and invitees: ${result.message}`
    case 'service_unavailable':
      return 'Could not create room. The selected account is not connected; try again after Axon reconnects.'
    default:
      return `Could not create room: ${result.message}`
  }
}

function emptyToNull(input: string): string | null {
  const trimmed = input.trim()
  return trimmed === '' ? null : trimmed
}

function loadRecentDirectoryServers(): string[] {
  const storage = browserLocalStorage()
  if (storage === null) {
    return []
  }
  return parseRecentDirectoryServers(
    storage.getItem(DIRECTORY_SERVER_RECENTS_KEY),
  )
}

function parseRecentDirectoryServers(raw: string | null): string[] {
  if (raw === null) {
    return []
  }
  try {
    const parsed = JSON.parse(raw)
    return Array.isArray(parsed)
      ? parsed.filter((server): server is string => typeof server === 'string')
      : []
  } catch {
    return []
  }
}

function rememberDirectoryServer(server: string): string[] | null {
  const recents = [
    server,
    ...loadRecentDirectoryServers().filter((recent) => recent !== server),
  ].slice(0, 5)
  const storage = browserLocalStorage()
  if (storage === null) {
    return null
  }
  try {
    storage.setItem(DIRECTORY_SERVER_RECENTS_KEY, JSON.stringify(recents))
  } catch {
    return null
  }
  return recents
}

function browserLocalStorage(): Storage | null {
  if (typeof window === 'undefined') {
    return null
  }
  let storage: Storage | undefined
  try {
    storage = window.localStorage
  } catch {
    return null
  }
  return storage !== undefined &&
    typeof storage.getItem === 'function' &&
    typeof storage.setItem === 'function'
    ? storage
    : null
}
