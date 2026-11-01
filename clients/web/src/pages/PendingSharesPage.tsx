import { useEffect, useState } from 'preact/hooks'
import { useServices } from '../services'
import { matchUrls } from '../html/linkify'
import { LinkPreviews } from '../components/LinkPreviews'
import { shareCompleted, shareOperations } from '../incoming-share/types'
import type { ShareOperation } from '../incoming-share/types'
import type { IncomingShareStore } from '../incoming-share/store'
import './pending-shares.css'

function RecoveryThumbnail({
  store,
  file,
  delivered,
}: {
  store: IncomingShareStore
  file: string
  delivered: boolean
}) {
  const [url, setUrl] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)
  useEffect(() => {
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
  }, [store, file])
  return failed ? (
    <p role="alert">
      {delivered
        ? 'The preview was delivered with the first item; its local image is unavailable.'
        : 'The retained image is missing or corrupt. Replace or remove it before sending.'}
    </p>
  ) : url === null ? (
    <p>Loading retained image…</p>
  ) : (
    <img
      class="pending-share-thumbnail"
      src={url}
      alt="Retained link preview"
      onError={() => setFailed(true)}
    />
  )
}

export function PendingSharesPage() {
  const { incomingShares: store, accounts, rooms, auth } = useServices()
  const [duplicateRisk, setDuplicateRisk] = useState(false)
  const selectedId = store?.selected.value?.id
  useEffect(() => setDuplicateRisk(false), [selectedId])
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
          disabled={busy || (item.state === 'unknown' && !duplicateRisk)}
          onClick={() => act(store.retry(item.id, duplicateRisk))}
        >
          {item.state === 'unknown' ? 'Allow explicit resend' : 'Prepare retry'}
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
  const deliveredOperations =
    record?.files.flatMap((file) =>
      file.operation === undefined ? [] : [file.operation],
    ) ?? []
  const complete = record !== null && shareCompleted(record)
  const captionSent =
    record !== null &&
    (deliveredOperations[0] ?? record.operation).state === 'sent'
  return (
    <section class="pending-shares-page">
      <h1>Pending shares</h1>
      <p>
        Saved on this device for up to 24 hours. Saving a share does not send
        it.
      </p>
      {store.error.value !== null && <p role="alert">{store.error.value}</p>}
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
      {record === null ? (
        <>
          {store.pending.value.length === 0 && <p>No pending shares.</p>}
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
                  onClick={() => act(store.open(item.id))}
                >
                  Review share
                </button>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => act(store.discard(item.id))}
                >
                  Discard share
                </button>
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
            <label>
              Message or first attachment caption
              <textarea
                key={record.id}
                value={store.editorText.value}
                onBlur={() => act(store.flushEdits())}
                onInput={(event) => act(store.edit(event.currentTarget.value))}
              />
            </label>
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
                  onClick={() => act(store.generate(preview.matched_url))}
                  disabled={record.accountId === undefined}
                >
                  Replace preview
                </button>
              </div>
            ))}
            <p>
              Include preview asks your homeserver to look up the URL,
              disclosing it to that server. Opening this review does not fetch
              the webpage.
            </p>
            {[
              ...new Set(
                matchUrls(store.editorText.value)
                  .filter((match) => /^https?:/.test(match.url))
                  .map((match) => match.url),
              ),
            ]
              .slice(0, 8)
              .map((url) => (
                <button
                  type="button"
                  key={url}
                  disabled={record.accountId === undefined}
                  onClick={() => act(store.generate(url))}
                >
                  Include preview: {url}
                </button>
              ))}
          </fieldset>
          {record.importFailures.length > 0 && (
            <p role="alert">
              Some items could not be imported. This share cannot be sent as a
              partial selection; discard it and import the intended selection
              again.
            </p>
          )}
          <ul aria-label="Delivery outcomes">
            {record.files.some((file) => !file.thumbnail)
              ? record.files
                  .filter((file) => !file.thumbnail)
                  .map((file) => operation(file.operation!, file.name))
              : operation(record.operation, 'Message')}
          </ul>
          <label>
            <input
              type="checkbox"
              checked={duplicateRisk}
              onChange={(event) =>
                setDuplicateRisk(event.currentTarget.checked)
              }
            />
            I understand an uncertain item may already have been delivered and
            resending can create a duplicate.
          </label>
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
