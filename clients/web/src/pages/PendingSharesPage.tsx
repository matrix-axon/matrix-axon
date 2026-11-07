import { useEffect, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { useServices } from '../services'
import { nativePreviewLinks } from '../incoming-share/urls'
import { LinkPreviews } from '../components/LinkPreviews'
import { shareCompleted, shareOperations } from '../incoming-share/types'
import type { ShareOperation } from '../incoming-share/types'
import type { IncomingShareStore } from '../incoming-share/store'
import './pending-shares.css'

function RecoveryThumbnail({
  store,
  file,
  delivered,
  photo = false,
}: {
  store: IncomingShareStore
  file: string
  delivered: boolean
  photo?: boolean
}) {
  const [url, setUrl] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)
  const [attempt, setAttempt] = useState(0)
  useEffect(() => {
    setFailed(false)
    setUrl(null)
    let active = true
    let objectUrl: string | null = null
    void store.thumbnail(file).then(
      (blob) => {
        if (!active) return
        objectUrl = URL.createObjectURL(blob)
        setUrl(objectUrl)
      },
      () => {
        if (active) setFailed(true)
      },
    )
    return () => {
      active = false
      if (objectUrl !== null) URL.revokeObjectURL(objectUrl)
    }
  }, [store, file, attempt])
  return failed ? (
    <div role="alert">
      <p>
        {photo
          ? 'Photo preview unavailable. The retained original is checked again before upload.'
          : delivered
            ? 'The preview was delivered with the shared message; its local image is unavailable.'
            : 'The retained image is missing or corrupt. Replace or remove it before sending.'}
      </p>
      <button type="button" onClick={() => setAttempt((value) => value + 1)}>
        Retry image preview
      </button>
    </div>
  ) : url === null ? (
    <p>Loading retained image…</p>
  ) : (
    <img
      class="pending-share-thumbnail"
      src={url}
      alt={photo ? 'Shared photo' : 'Retained link preview'}
      onError={() => setFailed(true)}
    />
  )
}

export function PendingSharesPage() {
  const location = useLocation()
  const { incomingShares: store, accounts, rooms, auth } = useServices()
  const [closing, setClosing] = useState(false)
  const [reviewId, setReviewId] = useState<string | null>(null)
  const [draggedPhoto, setDraggedPhoto] = useState<string | null>(null)
  const [messageEditorId, setMessageEditorId] = useState<string | null>(null)
  const [homeserverURL, setHomeserverURL] = useState<string | null>(null)
  const [clearIds, setClearIds] = useState<string[] | null>(null)
  const selectedId = store?.selected.value?.id
  useEffect(() => {
    setHomeserverURL(null)
  }, [selectedId])
  useEffect(() => {
    if (store == null) return
    void store.refresh()
    void accounts.refresh()
    void rooms.refresh()
    const heartbeat = setInterval(() => {
      void store.heartbeat().catch(() => {})
    }, 30_000)
    const persist = () => {
      void store.flushEdits().catch(() => {})
    }
    document.addEventListener('visibilitychange', persist)
    window.addEventListener('pagehide', persist)
    return () => {
      document.removeEventListener('visibilitychange', persist)
      window.removeEventListener('pagehide', persist)
      clearInterval(heartbeat)
      void store.close().catch(() => {})
    }
  }, [store, accounts, rooms])
  if (store == null)
    return (
      <section class="pending-shares-page">
        <h1>Pending shares</h1>
        <p>Incoming-share recovery is available in the iOS app.</p>
      </section>
    )
  const record = store.selected.value
  const busy = store.busy.value
  const reviewError =
    record === null && store.pending.value.some((item) => item.id === reviewId)
  const act = (action: Promise<unknown>) => {
    void action.catch(() => {})
  }
  const operation = (item: ShareOperation, name: string) => (
    <li key={item.id}>
      <strong>{name}</strong>:{' '}
      {item.state === 'unknown'
        ? 'Outcome unknown — delivery may have occurred.'
        : item.state}
      {item.eventId !== undefined && <span> Event: {item.eventId}</span>}
      {(item.state === 'failed' || item.state === 'unknown') && (
        <button
          type="button"
          disabled={busy}
          onClick={() => act(store.retry(item.id, true))}
        >
          {item.state === 'unknown' ? 'Retry item' : 'Prepare retry'}
        </button>
      )}
    </li>
  )
  const previews = record?.previews?.map((preview) => ({
    'matrix:matched_url': preview.matched_url,
    'og:title': preview.title,
    'og:description': preview.description,
    'og:site_name': preview.site_name,
  }))
  const previewLinks = [
    ...new Set(nativePreviewLinks(store.editorText.value)),
  ].filter((url) => /^https?:/i.test(url))
  const importProblems: Record<string, string> = {
    quota:
      'A source item or the selection exceeded the staging/storage limits.',
    too_many_photos: 'The selection exceeds the limit of 10 photos.',
    expired: 'A source took too long to provide its content.',
    provider_timeout: 'A source took too long to provide its content.',
    incomplete_import:
      'Import was interrupted before the selection was complete.',
    unsupported_input: 'The selection contains unsupported content.',
    unsupported_url: 'The selection contains an unsupported URL.',
    too_many_source_previews:
      'The source supplied more than eight link previews. Share fewer links.',
    invalid: 'A source item is invalid or could not be decoded.',
    unreadable_manifest:
      'Saved draft metadata could not be read. Check the destination before discarding it.',
    missingFile: 'A source file is missing or no longer readable.',
    provider_unavailable: 'The source app could not provide an item.',
    unavailable: 'The source app could not provide an item.',
  }
  const blocked =
    record !== null &&
    shareOperations(record).some((item) =>
      ['unknown', 'dispatching', 'failed', 'uploading'].includes(item.state),
    )
  const fixedDestination =
    record !== null &&
    shareOperations(record).some((item) =>
      ['sent', 'dispatching', 'unknown'].includes(item.state),
    )
  const complete = record !== null && shareCompleted(record)
  const captionSent = record?.operation.state === 'sent'
  return (
    <section class="pending-shares-page">
      <div class="pending-shares-header">
        <h1>Pending shares</h1>
        <button
          type="button"
          aria-label="Close pending shares"
          disabled={busy || closing}
          onClick={() => {
            setClosing(true)
            void store
              .close()
              .then((closed) => {
                if (closed) location.route('/')
              })
              .catch(() => {})
              .finally(() => setClosing(false))
          }}
        >
          <svg width="24" height="24" viewBox="0 0 24 24" aria-hidden="true">
            <path
              d="m6 6 12 12M18 6 6 18"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
            />
          </svg>
        </button>
      </div>
      <p>
        Saved on this device for up to 24 hours. Saving a share does not send
        it.
      </p>
      {store.error.value !== null && !reviewError && (
        <p role="alert">{store.error.value}</p>
      )}
      {store.sessionError.value !== null && (
        <p role="alert">{store.sessionError.value}</p>
      )}
      {store.notice.value !== null && <p role="status">{store.notice.value}</p>}
      <button
        type="button"
        disabled={busy}
        onClick={() => act(store.refresh())}
      >
        Refresh pending shares
      </button>
      <button
        type="button"
        disabled={busy}
        onClick={() => act(auth.refreshNow().then(() => store.refresh()))}
      >
        Refresh sign-in
      </button>
      {clearIds === null ? (
        store.pending.value.length > 0 && (
          <button
            type="button"
            class="danger"
            disabled={busy}
            onClick={() =>
              setClearIds(store.pending.value.map((item) => item.id))
            }
          >
            Clear all
          </button>
        )
      ) : (
        <div role="group" aria-label="Clear pending shares">
          <p>
            Delete {clearIds.length} saved{' '}
            {clearIds.length === 1 ? 'share' : 'shares'} from this device? This
            removes drafts and saved delivery records. Messages already sent
            remain in their rooms. Drafts open in another share window may also
            be removed. Shares currently sending will be kept.
          </p>
          <button
            type="button"
            class="danger"
            disabled={busy}
            onClick={() => {
              act(store.discardAll(clearIds))
              setClearIds(null)
            }}
          >
            Clear all pending shares
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => setClearIds(null)}
          >
            Cancel clear all
          </button>
        </div>
      )}
      {record === null ? (
        <>
          {store.pending.value.length === 0 && (
            <p>
              {store.unavailable.value
                ? 'Pending shares could not be loaded. Refresh to retry.'
                : 'No pending shares.'}
            </p>
          )}
          <ul>
            {store.pending.value.map((item) => (
              <li key={item.id}>
                <span>
                  {item.text.slice(0, 120) ||
                    `${item.files.filter((file) => !file.thumbnail).length} attachments`}
                </span>
                <span>
                  {' '}
                  Expires{' '}
                  {new Date(
                    (item.createdAt + 24 * 60 * 60) * 1000,
                  ).toLocaleString()}
                </span>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => {
                    setReviewId(item.id)
                    act(store.open(item.id))
                  }}
                >
                  {store.openingId.value === item.id
                    ? 'Opening share…'
                    : 'Review share'}
                </button>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => act(store.discard(item.id))}
                >
                  Discard share
                </button>
                {reviewId === item.id && store.error.value !== null && (
                  <p role="alert">{store.error.value}</p>
                )}
              </li>
            ))}
          </ul>
        </>
      ) : (
        <>
          <fieldset disabled={busy || fixedDestination}>
            <legend>Sending identity and destination</legend>
            <label>
              Account
              <select
                value={record.accountId ?? ''}
                onChange={(event) =>
                  act(store.destination(event.currentTarget.value))
                }
              >
                <option value="">Choose account</option>
                {accounts.accounts.value.map((account) => (
                  <option key={account.account_id} value={account.account_id}>
                    {account.user_id}
                  </option>
                ))}
              </select>
            </label>
            <label>
              Room
              <select
                value={record.roomId ?? ''}
                onChange={(event) =>
                  act(
                    store.destination(
                      record.accountId!,
                      event.currentTarget.value,
                    ),
                  )
                }
                disabled={record.accountId === undefined}
              >
                <option value="">Choose room</option>
                {rooms.rooms.value
                  .filter((room) => room.account_id === record.accountId)
                  .map((room) => (
                    <option key={room.room_id} value={room.room_id}>
                      {room.name ?? room.display_name ?? room.room_id}
                    </option>
                  ))}
              </select>
            </label>
            {(messageEditorId === record.id ||
              store.editorText.value !== '' ||
              !record.files.some((file) => !file.thumbnail)) && (
              <label>
                {record.files.some((file) => !file.thumbnail)
                  ? 'Shared message'
                  : 'Message'}
                <textarea
                  key={record.id}
                  onFocus={() => setMessageEditorId(record.id)}
                  value={store.editorText.value}
                  onBlur={() => act(store.flushEdits())}
                  onInput={(event) =>
                    act(store.edit(event.currentTarget.value))
                  }
                />
              </label>
            )}
            <LinkPreviews
              accountId={record.accountId ?? ''}
              body={store.editorText.value}
              content={{ 'm.url_previews': previews ?? [] }}
            />
            {record.previews?.map((preview) => (
              <div key={preview.matched_url}>
                {preview.thumbnail !== undefined && (
                  <RecoveryThumbnail
                    delivered={captionSent}
                    key={`${record.id}:${preview.thumbnail}`}
                    store={store}
                    file={preview.thumbnail}
                  />
                )}
                <span>
                  {preview.provenance === 'source'
                    ? 'Supplied preview'
                    : 'Generated preview'}
                  {preview.thumbnail !== undefined
                    ? ' · thumbnail retained on device'
                    : ''}
                </span>
                <button
                  type="button"
                  onClick={() => act(store.removePreview(preview.matched_url))}
                >
                  Remove preview
                </button>
                {(preview.thumbnail !== undefined ||
                  preview.image_upload_id != null) && (
                  <button
                    type="button"
                    onClick={() =>
                      act(store.removePreview(preview.matched_url, true))
                    }
                  >
                    Remove preview image
                  </button>
                )}
                <button
                  type="button"
                  disabled={!previewLinks.includes(preview.matched_url)}
                  onClick={() => act(store.generate(preview.matched_url))}
                >
                  Refresh on this device
                </button>
              </div>
            ))}
            <p>
              Include preview looks up the link on this device. The website and
              its image hosts may receive the request. Device previews require
              HTTPS; other links remain sendable.
            </p>
            {previewLinks.slice(0, 8).map((url) => (
              <button
                type="button"
                key={url}
                onClick={() => act(store.generate(url))}
              >
                Include preview: {url}
              </button>
            ))}
            <p>
              The homeserver option discloses the URL to your selected Matrix
              homeserver, which may log it even for encrypted rooms.
            </p>
            {previewLinks.slice(0, 8).map((url) => (
              <button
                type="button"
                key={url}
                disabled={record.accountId === undefined}
                onClick={() => setHomeserverURL(url)}
              >
                Use homeserver instead: {url}
              </button>
            ))}
            {homeserverURL !== null && previewLinks.includes(homeserverURL) && (
              <div role="group" aria-label="Confirm homeserver preview lookup">
                <p>
                  Send {homeserverURL} to your homeserver to fetch a preview? It
                  may record the URL in its logs.
                </p>
                <button
                  type="button"
                  onClick={() => {
                    const url = homeserverURL
                    setHomeserverURL(null)
                    act(store.generateUsingHomeserver(url))
                  }}
                >
                  Fetch using homeserver
                </button>
                <button type="button" onClick={() => setHomeserverURL(null)}>
                  Cancel preview lookup
                </button>
              </div>
            )}
          </fieldset>
          {record.files.some((file) => !file.thumbnail) && (
            <fieldset>
              <legend>Photos in sending order</legend>
              {record.files.filter((file) => !file.thumbnail).length > 1 && (
                <label>
                  Master caption
                  <textarea
                    class="pending-photo-caption"
                    rows={1}
                    placeholder="Master caption"
                    disabled={busy || fixedDestination}
                    value={store.masterCaption.value}
                    onInput={(event) =>
                      act(store.editMasterCaption(event.currentTarget.value))
                    }
                    onBlur={() => act(store.flushEdits())}
                  />
                </label>
              )}
              {record.files
                .filter((file) => !file.thumbnail)
                .map((file) => (
                  <div
                    key={file.id}
                    class="pending-photo-block"
                    onDragStart={(event) => {
                      setDraggedPhoto(file.id)
                      event.dataTransfer?.setData('text/plain', file.id)
                    }}
                    onDragEnd={() => setDraggedPhoto(null)}
                    onDragOver={(event) => {
                      if (draggedPhoto && !busy && !fixedDestination)
                        event.preventDefault()
                    }}
                    onDrop={(event) => {
                      event.preventDefault()
                      if (
                        !draggedPhoto ||
                        busy ||
                        fixedDestination ||
                        draggedPhoto === file.id
                      )
                        return
                      const order = record.files
                        .filter((photo) => !photo.thumbnail)
                        .map((photo) => photo.id)
                      order.splice(order.indexOf(draggedPhoto), 1)
                      order.splice(order.indexOf(file.id), 0, draggedPhoto)
                      setDraggedPhoto(null)
                      act(store.reorderPhotos(order))
                    }}
                  >
                    <RecoveryThumbnail
                      store={store}
                      file={file.id}
                      photo
                      delivered={file.operation?.state === 'sent'}
                    />
                    <p class="pending-photo-name">
                      {file.name}
                      {record.files.filter((photo) => !photo.thumbnail).length >
                        1 && (
                        <button
                          type="button"
                          draggable={!busy && !fixedDestination}
                          disabled={busy || fixedDestination}
                          class="pending-photo-handle"
                          aria-label={`Drag ${file.name} to reorder`}
                        >
                          ⠿
                        </button>
                      )}
                    </p>
                    <textarea
                      class="pending-photo-caption"
                      rows={1}
                      aria-label={`Caption for ${file.name}`}
                      placeholder="Caption"
                      disabled={busy || fixedDestination}
                      value={store.editorCaption(file.id)}
                      onBlur={() => act(store.flushEdits())}
                      onInput={(event) =>
                        act(
                          store.editPhotoCaption(
                            file.id,
                            event.currentTarget.value,
                          ),
                        )
                      }
                    />
                    {record.files.filter((photo) => !photo.thumbnail).length >
                      1 && (
                      <div class="pending-share-actions">
                        {[-1, 1].map((direction) => {
                          const order = record.files
                            .filter((photo) => !photo.thumbnail)
                            .map((photo) => photo.id)
                          const index = order.indexOf(file.id)
                          const target = index + direction
                          return (
                            <button
                              type="button"
                              disabled={
                                busy ||
                                fixedDestination ||
                                target < 0 ||
                                target >= order.length
                              }
                              aria-label={`Move ${file.name} ${direction < 0 ? 'up' : 'down'}`}
                              onClick={() => {
                                ;[order[index], order[target]] = [
                                  order[target],
                                  order[index],
                                ]
                                act(store.reorderPhotos(order))
                              }}
                            >
                              {direction < 0 ? '↑' : '↓'}
                            </button>
                          )
                        })}
                      </div>
                    )}
                    <p>
                      {Math.ceil(file.bytes / 1024)} KiB original
                      {file.photoSize !== undefined &&
                        file.photoSize !== 'original' && (
                          <>
                            {' '}
                            · {file.photoSize} · ~
                            {Math.ceil((file.photoBytes ?? file.bytes) / 1024)}{' '}
                            KiB upload
                          </>
                        )}
                    </p>
                    <button
                      type="button"
                      disabled={busy || fixedDestination}
                      onClick={() => act(store.removeFile(file.id))}
                    >
                      Remove photo: {file.name}
                    </button>
                  </div>
                ))}
            </fieldset>
          )}
          {record.importFailures.length > 0 && (
            <div role="alert">
              <p>
                Some items could not be imported. This share cannot be sent as a
                partial selection; discard it and import the intended selection
                again.
              </p>
              <ul>
                {[...new Set(record.importFailures)].map((failure) => (
                  <li key={failure}>
                    {importProblems[failure] ??
                      'A source item could not be imported. Share the intended selection again.'}
                  </li>
                ))}
              </ul>
            </div>
          )}
          <ul aria-label="Delivery outcomes">
            {record.text.trim() !== '' &&
              record.files.some((file) => !file.thumbnail) &&
              operation(record.operation, 'Shared message')}
            {record.files.some((file) => !file.thumbnail)
              ? record.files
                  .filter((file) => !file.thumbnail)
                  .map((file) => operation(file.operation!, file.name))
              : operation(record.operation, 'Message')}
          </ul>
          {store.error.value !== null && (
            <p class="pending-share-exit-error">{store.error.value}</p>
          )}
          <div class="pending-share-actions">
            {busy && (
              <button type="button" onClick={() => store.cancel()}>
                Stop after current request
              </button>
            )}
            <button
              type="button"
              disabled={
                busy ||
                blocked ||
                !record.accountId ||
                !record.roomId ||
                record.importFailures.length > 0
              }
              onClick={() => act(store.send())}
            >
              {busy
                ? 'Working…'
                : complete
                  ? 'Finish recovery'
                  : 'Send reviewed share'}
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => act(store.close())}
            >
              Keep draft
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => act(store.discard(record.id))}
            >
              Discard share
            </button>
          </div>
        </>
      )}
    </section>
  )
}
