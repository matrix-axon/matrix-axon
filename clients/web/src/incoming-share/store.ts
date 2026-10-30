import { signal } from '@preact/signals'
import type { ApiClient } from '../api/client'
import type { TimelineStoreCache } from '../stores/timeline-cache'
import type { SendProgress } from '../stores/timeline'
import { matchUrls } from '../html/linkify'
import { previewPreflight, type PreviewInput } from './contract'
import type {
  IncomingShare,
  IncomingSharePort,
  ShareOperation,
  SharePreview,
} from './types'

export type IncomingShareStore = ReturnType<typeof createIncomingShareStore>

export function createIncomingShareStore(
  port: IncomingSharePort,
  api: ApiClient,
  timelines: TimelineStoreCache,
  accessToken?: () => Promise<string | null>,
) {
  const pending = signal<IncomingShare[]>([])
  const selected = signal<IncomingShare | null>(null)
  const busy = signal(false)
  const error = signal<string | null>(null)
  let epoch = 0
  let tail: Promise<void> = Promise.resolve()
  let sessionReady = () => Promise.resolve()
  let stopRequested = false

  function describeFailure(cause: unknown) {
    const code =
      typeof cause === 'object' && cause !== null && 'code' in cause
        ? cause.code
        : null
    const messages: Record<string, string> = {
      missingFile:
        'The retained file is missing or corrupt. Replace or remove its preview, or discard this share.',
      quota:
        'A staging or storage limit was exceeded. Keep the draft, reduce its content, or explicitly discard another pending share.',
      invalid:
        'The share contains invalid or over-limit data. Review and edit it before sending.',
      busy: 'Another process still owns this share. Keep the draft and retry after it finishes.',
      stale:
        'The share changed or its ownership expired. Refresh pending shares before reviewing it again.',
      expired:
        'The request timed out. The draft is retained; inspect the saved delivery outcomes before retrying.',
    }
    error.value =
      typeof code === 'string' && messages[code] !== undefined
        ? messages[code]
        : 'Could not access this pending share. Open Axon, sign in or refresh your session, then retry.'
  }
  async function refresh() {
    const generation = epoch
    try {
      const records = await port.call<IncomingShare[]>('list')
      if (generation === epoch) pending.value = records
    } catch (cause) {
      if (generation === epoch) describeFailure(cause)
    }
  }
  function change(
    update: (record: IncomingShare) => IncomingShare,
    release = false,
  ): Promise<void> {
    const generation = epoch
    const id = selected.peek()?.id
    const task = tail.then(async () => {
      if (generation !== epoch) throw new Error('session changed')
      const record = selected.peek()
      if (record === null || record.id !== id) throw new Error('share changed')
      const next = await port.call<IncomingShare>('update', {
        manifest: update(structuredClone(record)),
        release,
      })
      if (generation !== epoch) throw new Error('session changed')
      selected.value = next
    })
    tail = task.catch((cause: unknown) => {
      if (generation === epoch) describeFailure(cause)
    })
    return task
  }
  function progress(fileId?: string): SendProgress {
    return (state, eventId) => {
      if (state === 'dispatching' && stopRequested)
        return Promise.reject(new Error('stopped before dispatch'))
      return change((record) => {
        const operation =
          fileId === undefined
            ? record.operation
            : record.files.find((file) => file.id === fileId)!.operation!
        operation.state = state
        operation.failure =
          state === 'failed'
            ? 'definite_rejection'
            : state === 'unknown'
              ? 'unconfirmed_dispatch'
              : undefined
        if (eventId !== undefined) operation.eventId = eventId
        return record
      })
    }
  }
  async function open(id: string) {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    try {
      if (selected.peek() !== null) await close(true)
      const record = await port.call<IncomingShare>('claim', { id })
      if (generation === epoch) {
        selected.value = record
        error.value = null
      }
    } catch (cause) {
      if (generation === epoch) describeFailure(cause)
    } finally {
      if (generation === epoch) busy.value = false
    }
  }
  async function close(fromOpen = false) {
    if (busy.peek() && !fromOpen) return
    if (selected.peek() === null) return
    const generation = epoch
    try {
      await change((record) => record, true)
    } finally {
      if (generation === epoch) {
        selected.value = null
        await refresh()
      }
    }
  }
  async function discard(id: string) {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    try {
      await tail
      const abandoned =
        selected.peek()?.id === id
          ? selected.peek()
          : pending.peek().find((record) => record.id === id)
      await port.call('discard', {
        id,
        owner: selected.peek()?.id === id ? selected.peek()?.owner : undefined,
      })
      if (abandoned !== undefined && abandoned !== null)
        void cleanup(abandoned, generation)
      if (generation === epoch) {
        if (selected.peek()?.id === id) selected.value = null
        await refresh()
      }
    } catch (cause) {
      if (generation === epoch) describeFailure(cause)
    } finally {
      if (generation === epoch) busy.value = false
    }
  }
  async function cleanup(record: IncomingShare, generation: number) {
    const operations = [
      record.operation,
      ...record.files.flatMap((file) =>
        file.operation === undefined ? [] : [file.operation],
      ),
    ]
    if (
      record.accountId === undefined ||
      operations.some((operation) =>
        ['unknown', 'dispatching'].includes(operation.state),
      )
    )
      return
    const ids = [
      ...new Set([
        ...(record.previews?.flatMap((preview) =>
          preview.image_upload_id == null ? [] : [preview.image_upload_id],
        ) ?? []),
        ...record.files.flatMap((file) =>
          file.operation?.uploadId === undefined ||
          file.operation.state === 'sent'
            ? []
            : [file.operation.uploadId],
        ),
      ]),
    ]
    if (ids.length === 0) return
    try {
      const token = await accessToken?.()
      if (generation !== epoch || (accessToken !== undefined && token == null))
        return
      await port.call('cleanup', {
        accountId: record.accountId,
        generation: record.session,
        uploadIds: ids,
        token,
      })
    } catch {
      /* Local discard is final; server staging has its own bounded TTL. */
    }
  }
  async function generate(url: string) {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    try {
      await tail
      const record = selected.peek()
      if (record?.accountId === undefined)
        throw new Error('destination required')
      const { data } = await api.POST(
        '/v1/accounts/{account_id}/link-previews/generate',
        {
          params: { path: { account_id: record.accountId } },
          body: { url },
        },
      )
      if (generation !== epoch) return
      const response = data?.data
      if (response?.status !== 'ready' || response.preview == null) {
        error.value =
          'Preview generation is unavailable. The original link can still be sent.'
        return
      }
      const preview: SharePreview = {
        ...response.preview,
        provenance: 'generated',
      }
      if (preview.image_upload_id != null) {
        const token = await accessToken?.()
        await sessionReady()
        if (generation !== epoch) return
        if (accessToken !== undefined && token == null)
          throw new Error('authentication required')
        const retained = await port.call<IncomingShare>('retain', {
          id: record.id,
          owner: record.owner,
          accountId: record.accountId,
          uploadId: preview.image_upload_id,
          token,
        })
        if (generation !== epoch) return
        const file = retained.files.at(-1)
        if (file === undefined) throw new Error('thumbnail missing')
        preview.thumbnail = file.id
        selected.value = retained
      }
      await change((latest) => ({
        ...latest,
        previews: [
          ...(latest.previews ?? []).filter((p) => p.matched_url !== url),
          preview,
        ],
      }))
      error.value = null
    } catch (cause) {
      if (generation === epoch) describeFailure(cause)
    } finally {
      if (generation === epoch) busy.value = false
    }
  }
  async function upload(fileId: string): Promise<string> {
    const current = epoch
    const token = await accessToken?.()
    await sessionReady()
    if (current !== epoch || (accessToken !== undefined && token == null))
      throw new Error('authentication required')
    const record = selected.peek()!
    const generation = epoch
    const result = await port.call<{ uploadId: string }>('upload', {
      id: record.id,
      owner: record.owner,
      accountId: record.accountId,
      file: fileId,
      token,
    })
    if (generation !== epoch) throw new Error('session changed')
    return result.uploadId
  }
  async function send() {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    error.value = null
    stopRequested = false
    try {
      await tail
      await sessionReady()
      let record = selected.peek()
      if (record !== null) {
        const saved = record.files.flatMap((file) =>
          file.operation === undefined ? [] : [file.operation],
        )
        if (
          (saved.length === 0 ? [record.operation] : saved).every(
            (operation) => operation.state === 'sent',
          )
        ) {
          await port.call('ack', { id: record.id, owner: record.owner })
          if (generation === epoch) {
            selected.value = null
            await refresh()
          }
          return
        }
      }
      if (
        record?.accountId === undefined ||
        record.roomId === undefined ||
        record.importFailures.length > 0
      )
        throw new Error('incomplete selection')
      // Confirm against current joined rooms, independent of cached picker names.
      const rooms = await api.GET('/v1/rooms')
      if (generation !== epoch) return
      if (
        !rooms.data?.data.some(
          (room) =>
            room.room_id === record!.roomId &&
            room.account_id === record!.accountId,
        )
      )
        throw new Error('room unavailable')
      const attachments = record.files.filter((file) => !file.thumbnail)
      const captionPending =
        attachments.length === 0
          ? record.operation.state !== 'sent'
          : attachments[0].operation?.state !== 'sent'
      const operations =
        attachments.length === 0
          ? [record.operation]
          : attachments.map((file) => file.operation!)
      if (
        operations.some((operation) =>
          ['unknown', 'dispatching', 'failed'].includes(operation.state),
        )
      ) {
        error.value = 'Resolve each failed or uncertain item before sending.'
        return
      }
      const wire: PreviewInput[] | undefined = record.previews?.map(
        ({
          matched_url,
          title,
          description,
          site_name,
          image_upload_id,
          thumbnail,
        }) => ({
          matched_url,
          title,
          description,
          site_name,
          // Reserve an image descriptor even when its server upload has expired.
          image_upload_id:
            thumbnail === undefined
              ? image_upload_id
              : '00000000-0000-4000-8000-000000000001',
        }),
      )
      let budget = ''
      const failure = captionPending
        ? previewPreflight(
            { body: record.text, url_previews: wire ?? [] },
            attachments.length > 0,
            (field, measured, limit) => {
              budget = `${field}: ${measured} / ${limit}. `
            },
          )
        : null
      if (failure !== null) {
        error.value = `Cannot send: ${failure}. ${budget}Shorten the text or remove excess previews. Your draft is retained.`
        return
      }
      // Retained thumbnails are always re-uploaded on an explicit Send. This
      // avoids relying on a server handle's independent TTL and never refetches a page.
      for (const preview of captionPending ? (record.previews ?? []) : []) {
        if (stopRequested) return
        if (preview.thumbnail !== undefined) {
          const uploadId = await upload(preview.thumbnail)
          await change((latest) => ({
            ...latest,
            previews: latest.previews?.map((p) =>
              p.matched_url === preview.matched_url
                ? { ...p, image_upload_id: uploadId }
                : p,
            ),
          }))
        } else if (preview.image_upload_id != null) {
          error.value =
            'This preview has no retained thumbnail. Explicitly replace or remove its image before sending.'
          return
        }
      }
      record = selected.peek()!
      if (stopRequested) return
      const previews = record.previews?.map(
        ({ matched_url, title, description, site_name, image_upload_id }) => ({
          matched_url,
          title,
          description,
          site_name,
          image_upload_id,
        }),
      )
      const timeline = timelines.acquire(record.accountId!, record.roomId!)
      let success = true
      if (attachments.length === 0) {
        if (record.operation.state !== 'sent')
          success = await timeline.send(record.text, {
            formattedBody: null,
            urlPreviews: previews,
            progress: progress(),
          })
      } else {
        for (const [index, file] of attachments.entries()) {
          if (stopRequested) {
            success = false
            break
          }
          if (file.operation!.state === 'sent') continue
          await change((latest) => {
            latest.files.find(
              (candidate) => candidate.id === file.id,
            )!.operation!.state = 'uploading'
            return latest
          })
          let uploadId: string
          try {
            uploadId = await upload(file.id)
          } catch (cause) {
            await change((latest) => {
              latest.files.find(
                (candidate) => candidate.id === file.id,
              )!.operation!.state = 'failed'
              return latest
            })
            throw cause
          }
          await change((latest) => {
            Object.assign(
              latest.files.find((candidate) => candidate.id === file.id)!
                .operation!,
              { state: 'uploaded', uploadId },
            )
            return latest
          })
          if (stopRequested) {
            success = false
            break
          }
          success = await timeline.sendStagedMedia(
            uploadId,
            index === 0 ? record.text : '',
            {
              urlPreviews: index === 0 ? previews : undefined,
              progress: progress(file.id),
            },
          )
          if (!success) break
        }
      }
      if (generation !== epoch) return
      if (success) {
        await port.call('ack', { id: record.id, owner: record.owner })
        if (generation === epoch) selected.value = null
        error.value = null
      } else
        error.value =
          'Delivery was interrupted. Review the saved outcomes before retrying.'
      await refresh()
    } catch (cause) {
      if (generation === epoch) describeFailure(cause)
    } finally {
      if (generation === epoch) busy.value = false
    }
  }
  return {
    pending,
    selected,
    busy,
    error,
    refresh,
    open,
    close,
    discard,
    generate,
    send,
    cancel() {
      stopRequested = true
      error.value =
        'Stopping after the current request. A message already dispatched may still be delivered; review the saved outcomes.'
    },
    async thumbnail(fileId: string): Promise<Blob> {
      const record = selected.peek()
      const generation = epoch
      const file = record?.files.find(
        (candidate) => candidate.id === fileId && candidate.thumbnail,
      )
      if (
        record === null ||
        file === undefined ||
        !['image/png', 'image/jpeg', 'image/webp'].includes(file.mime) ||
        file.bytes <= 0 ||
        file.bytes > 2 * 1024 * 1024
      )
        throw new Error('thumbnail unavailable')
      const data = new Uint8Array(file.bytes)
      for (let offset = 0; offset < file.bytes; offset += 64 * 1024) {
        const result = await port.call<{ bytes: string }>('read', {
          id: record.id,
          owner: record.owner,
          file: fileId,
          offset,
          count: Math.min(64 * 1024, file.bytes - offset),
        })
        if (generation !== epoch || selected.peek()?.id !== record.id)
          throw new Error('session changed')
        const chunk = Uint8Array.from(atob(result.bytes), (character) =>
          character.charCodeAt(0),
        )
        if (chunk.length !== Math.min(64 * 1024, file.bytes - offset))
          throw new Error('thumbnail unavailable')
        data.set(chunk, offset)
      }
      const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', data))
      if (
        [...digest]
          .map((byte) => byte.toString(16).padStart(2, '0'))
          .join('') !== file.digest
      )
        throw new Error('thumbnail corrupt')
      return new Blob([data], { type: file.mime })
    },
    async invalidateAccount(accountId: string) {
      if (selected.peek()?.accountId === accountId) {
        epoch += 1
        selected.value = null
        busy.value = false
      }
      await port.call('clear_account', { accountId })
      await refresh()
    },
    setSessionReady(wait: () => Promise<void>) {
      sessionReady = wait
    },
    async edit(text: string) {
      const urls = new Set(matchUrls(text).map((match) => match.url))
      await change((record) => ({
        ...record,
        text,
        previews: record.previews?.filter((p) => urls.has(p.matched_url)),
      }))
    },
    async destination(accountId: string, roomId?: string) {
      await change((record) => ({
        ...record,
        accountId,
        roomId,
        previews:
          record.accountId === accountId
            ? record.previews
            : record.previews?.map((p) => ({
                ...p,
                image_upload_id: undefined,
              })),
      }))
    },
    async removePreview(url: string, imageOnly = false) {
      await change((record) => ({
        ...record,
        previews: imageOnly
          ? record.previews?.map((p) =>
              p.matched_url === url
                ? { ...p, image_upload_id: undefined, thumbnail: undefined }
                : p,
            )
          : record.previews?.filter((p) => p.matched_url !== url),
      }))
    },
    async retry(operationId: string, acceptDuplicateRisk: boolean) {
      await change((record) => {
        const operations: ShareOperation[] = [
          record.operation,
          ...record.files.flatMap((file) =>
            file.operation === undefined ? [] : [file.operation],
          ),
        ]
        const operation = operations.find(
          (candidate) => candidate.id === operationId,
        )
        if (
          operation?.state === 'failed' ||
          (operation?.state === 'unknown' && acceptDuplicateRisk)
        )
          operation.state = 'ready'
        return record
      })
    },
    async heartbeat() {
      if (!busy.peek() && selected.peek() !== null)
        await change((record) => record)
    },
    reset() {
      stopRequested = true
      epoch += 1
      selected.value = null
      pending.value = []
      busy.value = false
      error.value = null
    },
  }
}
