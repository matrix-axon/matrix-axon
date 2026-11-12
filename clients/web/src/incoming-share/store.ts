import { computed, signal } from '@preact/signals'
import type { ApiClient } from '../api/client'
import type { TimelineStoreCache } from '../stores/timeline-cache'
import type { SendProgress } from '../stores/timeline'
import { serverBodyUrls } from '../html/linkify'
import { nativePreviewLinks } from '../preview-links'
import { previewPreflight, type PreviewInput } from './contract'
import {
  canonicalShare,
  photoCaption,
  shareCompleted,
  shareOperations,
} from './types'
import type {
  IncomingShare,
  IncomingSharePort,
  ShareOperation,
  SharePreview,
} from './types'

export type IncomingShareStore = ReturnType<typeof createIncomingShareStore>

export function createIncomingShareStore(
  incomingPort: IncomingSharePort,
  api: ApiClient,
  timelines: TimelineStoreCache,
  accessToken?: () => Promise<string | null>,
) {
  const normalize = (value: unknown): unknown => {
    if (Array.isArray(value)) return value.map(normalize)
    if (
      value &&
      typeof value === 'object' &&
      'version' in value &&
      'files' in value &&
      'text' in value
    )
      return canonicalShare(value as IncomingShare)
    return value
  }
  const port: IncomingSharePort = {
    subscribe: (listener) => incomingPort.subscribe(listener),
    call: async <T>(
      action: string,
      args?: Record<string, unknown>,
    ): Promise<T> => normalize(await incomingPort.call(action, args)) as T,
  }
  const pending = signal<IncomingShare[]>([])
  const selected = signal<IncomingShare | null>(null)
  const busy = signal(false)
  const error = signal<string | null>(null)
  const notice = signal<string | null>(null)
  const unavailable = signal(false)
  const sessionError = signal<string | null>(null)
  type DeviceRequest = {
    id: string
    cancelled: boolean
    started: boolean
    done: Promise<void>
  }
  let deviceRequest: DeviceRequest | null = null
  let previewWork: Promise<void> | null = null
  let thumbnailWork: Promise<void> = Promise.resolve()
  function cancelPreview() {
    const request = deviceRequest
    if (request !== null) request.cancelled = true
    if (request?.started)
      void port
        .call('cancel_preview', { requestId: request.id })
        .catch(() => {})
    return request?.done ?? Promise.resolve()
  }
  let epoch = 0
  let refreshVersion = 0
  let tail: Promise<void> = Promise.resolve()
  let sessionReady = () => Promise.resolve()
  let stopRequested = false
  type CaptionDraft = { text?: string; captions?: Record<string, string> }
  const drafts = signal<Record<string, CaptionDraft>>({})
  const editorText = computed(() => {
    const record = selected.value
    return record === null ? '' : (drafts.value[record.id]?.text ?? record.text)
  })
  let flushingEdit:
    { id: string; draft: CaptionDraft; promise: Promise<void> } | undefined
  let editTimer: ReturnType<typeof setTimeout> | undefined
  let editDeadline: ReturnType<typeof setTimeout> | undefined
  const problem = (code: string) => Object.assign(new Error(code), { code })
  function clearEditTimers() {
    clearTimeout(editTimer)
    clearTimeout(editDeadline)
    editTimer = undefined
    editDeadline = undefined
  }
  async function flushEdits() {
    clearEditTimers()
    const id = selected.peek()?.id
    if (id === undefined) return
    const draft = drafts.peek()[id]
    if (draft === undefined) return
    if (flushingEdit?.id === id && flushingEdit.draft === draft)
      return flushingEdit.promise
    const promise = change((record) => {
      const text = draft.text ?? record.text
      const urls = serverBodyUrls(text)
      return {
        ...record,
        version: 4,
        text,
        files: record.files.map((file) =>
          draft.captions?.[file.id] === undefined
            ? file
            : { ...file, caption: draft.captions[file.id] },
        ),
        previews: record.previews?.filter((preview) =>
          urls.has(preview.matched_url),
        ),
      }
    })
    flushingEdit = { id, draft, promise }
    try {
      await promise
    } finally {
      if (flushingEdit?.promise === promise) flushingEdit = undefined
    }
    if (drafts.peek()[id] === draft) {
      const next = { ...drafts.peek() }
      delete next[id]
      drafts.value = next
    }
  }

  function scheduleEdit(id: string, draft: CaptionDraft) {
    drafts.value = { ...drafts.peek(), [id]: draft }
    clearTimeout(editTimer)
    const persist = () => {
      void flushEdits().catch(() => {})
    }
    editTimer = setTimeout(persist, 250)
    editDeadline ??= setTimeout(persist, 1000)
  }
  function editorCaption(file: string): string {
    const record = selected.value
    const photo = record?.files.find((item) => item.id === file)
    if (!record || !photo) return ''
    return (
      drafts.value[record.id]?.captions?.[file] ?? photoCaption(record, photo)
    )
  }

  function failureCode(cause: unknown): string | null {
    return typeof cause === 'object' &&
      cause !== null &&
      'code' in cause &&
      typeof cause.code === 'string'
      ? cause.code
      : null
  }
  function failureMessage(cause: unknown, clearing = false): string {
    const code = failureCode(cause)
    const messages: Record<string, string> = {
      destination: 'Choose a sending account and joined room before sending.',
      incomplete:
        'Some items could not be imported. Discard this share and import the intended selection again.',
      room: 'This room is no longer available for the selected account. Choose a current joined room.',
      authentication:
        'Open Axon and sign in or refresh your session before resuming.',
      httpsRequired:
        'Native sharing requires an HTTPS server. This draft is retained; configure HTTPS before sending.',
      invalidation:
        'Could not disable shared access. Keep Axon open and retry signing out.',
      missingFile:
        'The retained file is missing or corrupt. Replace or remove its preview, or discard this share.',
      quota:
        'A staging or storage limit was exceeded. Keep the draft, reduce its content, or explicitly discard another pending share.',
      pendingLimit:
        'Pending shares is full. Send or explicitly discard an existing draft, then share again from the source.',
      devicePreview_unavailable:
        'The on-device preview lookup is unavailable. The link remains sendable; you can explicitly choose Use homeserver instead.',
      devicePreview_timeout:
        'The on-device preview lookup timed out. The link remains sendable; you can explicitly choose Use homeserver instead.',
      devicePreview_noMetadata:
        'The device found no usable preview metadata. The original link remains sendable.',
      devicePreview_publicIPv4Required:
        // Also a name that does not exist, which no resolver tells apart
        // from an IPv6-only site (issue 1059), so the copy names both.
        'No IPv4 address was found for this site. Check the link; on-device previews also cannot reach sites that only have IPv6 addresses. The link remains sendable; you can explicitly choose Use homeserver instead.',
      invalid:
        'The share contains invalid or over-limit data. Review and edit it before sending.',
      busy: 'Another share operation is active. Close other Share → Axon composers, wait up to five minutes, then retry.',
      sending:
        'A share is still sending. Wait for it to finish, then refresh and retry clearing it.',
      previewBusy:
        'Another on-device preview is finishing. Wait a moment and try again.',
      transferBusy: 'Sharing transfers are busy. Wait a moment and try again.',
      devicePreview_unsupportedURL:
        'This link cannot be previewed on this device. The original link remains sendable.',
      cancelled:
        'Preview lookup cancelled. The original link remains sendable.',
      stale:
        'The share changed or its ownership expired. Refresh pending shares before reviewing it again.',
      expired:
        'The request timed out. The draft is retained; inspect the saved delivery outcomes before retrying.',
    }
    if (clearing && code === 'busy') {
      return 'Saved shares are temporarily busy. Wait a moment and retry clearing.'
    }
    return typeof code === 'string' && messages[code] !== undefined
      ? messages[code]
      : 'Could not complete this operation. The draft is retained; refresh pending shares and retry.'
  }
  function describeFailure(cause: unknown, clearing = false) {
    error.value = failureMessage(cause, clearing)
  }
  async function refresh() {
    const generation = epoch
    const version = ++refreshVersion
    const previousReadFailure = unavailable.peek() ? error.peek() : null
    const reviewed = selected.peek()
    let reclaiming = false
    let listed = false
    try {
      await tail
      const records = await port.call<IncomingShare[]>('list')
      if (generation !== epoch || version !== refreshVersion) return
      listed = true
      unavailable.value = false
      if (previousReadFailure !== null && error.peek() === previousReadFailure)
        error.value = null
      pending.value = records
      const current = selected.peek()
      if (busy.peek() || current === null) return
      if (current.id !== reviewed?.id || current.owner !== reviewed?.owner)
        return
      const fresh = records.find((record) => record.id === current.id)
      if (fresh !== undefined && fresh.owner === current.owner) {
        if (fresh.revision > current.revision) selected.value = fresh
        return
      }
      selected.value = null
      if (fresh !== undefined && fresh.owner === undefined) {
        reclaiming = true
        busy.value = true
        const reclaimed = await port.call<IncomingShare>('claim', {
          id: current.id,
        })
        if (generation === epoch && selected.peek() === null) {
          selected.value = reclaimed
          error.value =
            'Ownership renewed. Review the saved outcomes and draft before sending.'
        }
      } else {
        error.value =
          'This share changed or another process owns it. Unsaved edits are kept here; reopen the share when it is available.'
      }
    } catch (cause) {
      if (generation === epoch && version === refreshVersion) {
        if (!listed) unavailable.value = true
        describeFailure(cause)
      }
    } finally {
      if (reclaiming && generation === epoch) busy.value = false
    }
  }
  function change(
    update: (record: IncomingShare) => IncomingShare,
    release = false,
    removeFile?: string,
    order?: string[],
  ): Promise<void> {
    const generation = epoch
    const id = selected.peek()?.id
    const task = tail.then(async () => {
      if (generation !== epoch) throw problem('stale')
      const record = selected.peek()
      if (record === null || record.id !== id) throw problem('stale')
      const next = await port.call<IncomingShare>(
        order !== undefined
          ? 'reorder_files'
          : removeFile === undefined
            ? 'update'
            : 'remove_file',
        {
          manifest: update(structuredClone(record)),
          release,
          ...(order === undefined ? {} : { order }),
          ...(removeFile === undefined ? {} : { file: removeFile }),
        },
      )
      if (generation !== epoch) throw problem('stale')
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
  let opening: Promise<void> | undefined
  let closing: Promise<boolean> | undefined
  const openingId = signal<string | null>(null)
  async function performOpen(id: string) {
    await closing?.catch(() => {})
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    openingId.value = id
    error.value = null
    try {
      await releaseSelection()
      const record = await port.call<IncomingShare>('claim', { id })
      if (generation === epoch) selected.value = record
    } catch (cause) {
      if (generation === epoch) describeFailure(cause)
    } finally {
      if (generation === epoch) {
        busy.value = false
        openingId.value = null
      }
    }
  }
  function open(id: string) {
    if (opening) return opening
    const task = performOpen(id)
    opening = task
    void task.finally(() => {
      if (opening === task) opening = undefined
    })
    return task
  }
  async function releaseSelection() {
    const generation = epoch
    if (selected.peek() === null) return
    await flushEdits()
    await change((record) => record, true)
    if (generation === epoch) selected.value = null
  }
  function close() {
    if (closing) return closing
    // Navigation may unmount the page while claim is still in flight.
    // Release that claim too; otherwise it remains locked for five minutes.
    const claim = opening
    const generation = epoch
    const task = (async () => {
      await claim
      const work = previewWork
      await cancelPreview()
      if (work !== null) await work
      if (generation !== epoch || busy.peek()) return false
      busy.value = true
      try {
        await releaseSelection()
        await refresh()
        return true
      } finally {
        if (generation === epoch) busy.value = false
      }
    })()
    closing = task
    void task
      .finally(() => {
        if (closing === task) closing = undefined
      })
      .catch(() => {})
    return task
  }
  async function discardRecord(record: IncomingShare, generation: number) {
    if (generation !== epoch) return
    const id = record.id
    // This is an explicit user discard, not an old composer's completion.
    // The native lock fences idle writers and protects any live dispatch.
    await port.call('discard', { id })
    if (generation !== epoch) return
    void cleanup(record, generation)
    const kept = { ...drafts.peek() }
    delete kept[id]
    drafts.value = kept
    if (selected.peek()?.id === id) selected.value = null
    pending.value = pending.peek().filter((item) => item.id !== id)
  }
  async function discard(id: string) {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    try {
      if (selected.peek()?.id === id) clearEditTimers()
      await tail
      const abandoned =
        selected.peek()?.id === id
          ? selected.peek()
          : pending.peek().find((record) => record.id === id)
      if (abandoned != null) await discardRecord(abandoned, generation)
      if (generation === epoch) {
        await refresh()
      }
    } catch (cause) {
      if (generation === epoch) describeFailure(cause, true)
    } finally {
      if (generation === epoch) busy.value = false
    }
  }
  async function discardAll(ids = pending.peek().map((record) => record.id)) {
    if (busy.peek() || ids.length === 0) return
    const generation = epoch
    const targets = new Set(ids)
    busy.value = true
    error.value = null
    notice.value = null
    // Do not let an older inbox read repaint entries removed by this batch.
    refreshVersion += 1
    if (targets.has(selected.peek()?.id ?? '')) clearEditTimers()
    let cleared = 0
    let failed = 0
    const failures = new Set<string>()
    try {
      await tail
      if (generation !== epoch) return
      const records = await port.call<IncomingShare[]>('list')
      if (generation !== epoch) return
      // Each discard uses the native ownership lock. Another active composer
      // keeps its dispatch outcome; one rejection must not stop other deletions.
      for (const record of records) {
        if (generation !== epoch) return
        if (!targets.has(record.id)) continue
        try {
          await discardRecord(record, generation)
          if (generation !== epoch) return
          cleared += 1
        } catch (cause) {
          if (generation !== epoch) return
          failed += 1
          failures.add(failureMessage(cause, true))
          if (!['busy', 'sending'].includes(failureCode(cause) ?? '')) {
            console.warn('Incoming share discard failed', {
              code: failureCode(cause) ?? 'unknown',
            })
          }
        }
      }
      await refresh()
      if (generation !== epoch) return
      notice.value = `Cleared ${cleared} pending ${cleared === 1 ? 'share' : 'shares'}.`
      if (failed > 0) {
        const summary = `${failed} ${failed === 1 ? 'share could' : 'shares could'} not be cleared.`
        // Refresh may clear an older error or add its own read failure. Keep
        // this batch's reasons independent of that shared UI signal.
        const refreshFailure = unavailable.peek() ? error.peek() : null
        if (refreshFailure !== null) failures.add(refreshFailure)
        error.value = [summary, ...failures].join(' ')
      }
    } catch (cause) {
      if (generation === epoch) describeFailure(cause, true)
    } finally {
      if (generation === epoch) busy.value = false
    }
  }
  async function cleanup(record: IncomingShare, generation: number) {
    const operations = shareOperations(record)
    if (
      record.accountId === undefined ||
      operations.some((operation) =>
        ['unknown', 'dispatching'].includes(operation.state),
      )
    )
      return
    const previewUploads =
      record.operation.state === 'sent'
        ? []
        : (record.previews?.flatMap((preview) =>
            preview.image_upload_id == null ? [] : [preview.image_upload_id],
          ) ?? [])
    const ids = [
      ...new Set([
        ...previewUploads,
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
  function generate(url: string, homeserver = false): Promise<void> {
    if (busy.peek()) return Promise.resolve()
    if (homeserver) return performGenerate(url, true, null)
    const request: DeviceRequest = {
      id: crypto.randomUUID(),
      cancelled: false,
      started: false,
      done: Promise.resolve(),
    }
    deviceRequest = request
    const task = performGenerate(url, false, request)
    const done = task.finally(() => {
      if (previewWork === done) previewWork = null
      if (deviceRequest === request) deviceRequest = null
    })
    request.done = done
    previewWork = done
    return done
  }
  async function performGenerate(
    url: string,
    homeserver: boolean,
    request: DeviceRequest | null,
  ) {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    try {
      await flushEdits()
      await tail
      const record = selected.peek()
      if (record === null || record.owner === undefined) throw problem('stale')
      if (
        (record.previews?.length ?? 0) >= 8 &&
        !record.previews?.some((p) => p.matched_url === url)
      )
        throw problem('quota')
      if (!nativePreviewLinks(record.text).includes(url))
        throw problem('devicePreview_unsupportedURL')
      if (!homeserver) {
        if (request === null) throw problem('stale')
        const task = tail.then(async () => {
          if (generation !== epoch) throw problem('stale')
          if (request.cancelled) throw problem('cancelled')
          request.started = true
          const result = await port.call<IncomingShare>('device_preview', {
            id: record.id,
            owner: record.owner,
            accountId: record.accountId,
            url,
            requestId: request.id,
            refresh:
              record.previews?.some((preview) => preview.matched_url === url) ??
              false,
          })
          if (
            generation === epoch &&
            selected.peek()?.id === record.id &&
            selected.peek()?.owner === record.owner
          ) {
            selected.value = result
            error.value = null
          }
        })
        tail = task.catch(() => {})
        await task
        return
      }
      if (!record.accountId) throw problem('destination')
      await port.call('check_transport')
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
          throw problem('authentication')
        const retained = await port.call<IncomingShare>('retain', {
          id: record.id,
          owner: record.owner,
          accountId: record.accountId,
          uploadId: preview.image_upload_id,
          preview,
          token,
        })
        if (generation !== epoch) return
        selected.value = retained
      } else {
        await change((latest) => ({
          ...latest,
          previews: [
            ...(latest.previews ?? []).filter((p) => p.matched_url !== url),
            preview,
          ],
        }))
      }
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
      throw problem('authentication')
    const record = selected.peek()!
    const generation = epoch
    const result = await port.call<{ uploadId: string }>('upload', {
      id: record.id,
      owner: record.owner,
      accountId: record.accountId,
      file: fileId,
      token,
    })
    if (generation !== epoch) throw problem('stale')
    return result.uploadId
  }
  async function send() {
    if (busy.peek()) return
    const generation = epoch
    busy.value = true
    error.value = null
    stopRequested = false
    try {
      await flushEdits()
      await tail
      await sessionReady()
      let record = selected.peek()
      if (record !== null) {
        if (shareCompleted(record)) {
          await port.call('ack', { id: record.id, owner: record.owner })
          if (generation === epoch) {
            selected.value = null
            await refresh()
          }
          return
        }
      }
      if (!record?.accountId || !record.roomId) throw problem('destination')
      if (record.importFailures.length > 0) throw problem('incomplete')
      await port.call('check_transport')
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
        throw problem('room')
      const attachments = record.files.filter((file) => !file.thumbnail)
      const messagePending =
        (record.text.trim() !== '' || attachments.length === 0) &&
        record.operation.state !== 'sent'
      const operations = shareOperations(record)
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
      const failure = messagePending
        ? previewPreflight(
            { body: record.text, url_previews: wire ?? [] },
            false,
            (field, measured, limit) => {
              budget = `${field}: ${measured} / ${limit}. `
            },
          )
        : null
      const photoFailure = attachments
        .filter((file) => file.operation?.state !== 'sent')
        .map((file) =>
          previewPreflight(
            {
              body: photoCaption(record!, file),
              url_previews: [],
            },
            true,
          ),
        )
        .find((value) => value !== null)
      if (failure !== null || photoFailure !== undefined) {
        error.value = `Cannot send: ${failure ?? photoFailure}. ${budget}Shorten the text or remove excess previews. Your draft is retained.`
        return
      }
      // Retained thumbnails are always re-uploaded on an explicit Send. This
      // avoids relying on a server handle's independent TTL and never refetches a page.
      for (const preview of messagePending ? (record.previews ?? []) : []) {
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
      if (messagePending) {
        success = await timeline.send(record.text, {
          formattedBody: null,
          urlPreviews: previews,
          progress: progress(),
        })
      }
      if (success) {
        for (const file of attachments) {
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
            photoCaption(record, file),
            {
              urlPreviews: undefined,
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
  const unsubscribe = port.subscribe(() => {
    void sessionReady().then(
      () => refresh(),
      () => {},
    )
  })
  return {
    dispose() {
      void cancelPreview()
      unsubscribe()
      clearEditTimers()
      epoch += 1
    },
    pending,
    selected,
    editorText,
    editorCaption,
    flushEdits,
    busy,
    error,
    notice,
    unavailable,
    sessionError,
    refresh,
    open,
    openingId,
    masterCaption: computed(() => {
      const photos =
        selected.value?.files.filter((file) => !file.thumbnail) ?? []
      const captions = photos.map((file) => editorCaption(file.id))
      return captions.every((caption) => caption === captions[0])
        ? (captions[0] ?? '')
        : ''
    }),
    async editMasterCaption(caption: string) {
      const record = selected.peek()
      if (!record || busy.peek()) return
      scheduleEdit(record.id, {
        ...drafts.peek()[record.id],
        captions: Object.fromEntries(
          record.files
            .filter((file) => !file.thumbnail)
            .map((file) => [file.id, caption]),
        ),
      })
    },
    async reorderPhotos(order: string[]) {
      if (busy.peek()) return
      await flushEdits()
      await change((record) => record, false, undefined, order)
    },
    close,
    discard,
    discardAll,
    generate,
    generateUsingHomeserver: (url: string) => generate(url, true),
    send,
    cancel() {
      void cancelPreview()
      stopRequested = true
      error.value =
        'Stopping after the current request. A message already dispatched may still be delivered; review the saved outcomes.'
    },
    async thumbnail(fileId: string): Promise<Blob> {
      const record = selected.peek()
      const generation = epoch
      const file = record?.files.find((candidate) => candidate.id === fileId)
      if (
        record === null ||
        file === undefined ||
        !file.mime.startsWith('image/') ||
        file.bytes <= 0 ||
        (file.thumbnail && file.bytes > 2 * 1024 * 1024)
      )
        throw new Error('thumbnail unavailable')
      if (!file.thumbnail) {
        // Ten photo views can mount together. Admit one decode at a time,
        // leaving the native transfer pool available for a concurrent send.
        const task = thumbnailWork.then(async () => {
          if (
            generation !== epoch ||
            selected.peek()?.id !== record.id ||
            selected.peek()?.owner !== record.owner
          )
            throw problem('stale')
          return port.call<{ bytes: string }>('photo_thumbnail', {
            id: record.id,
            owner: record.owner,
            file: fileId,
          })
        })
        thumbnailWork = task.then(
          () => {},
          () => {},
        )
        const result = await task
        if (generation !== epoch || selected.peek()?.id !== record.id)
          throw problem('stale')
        if (typeof result.bytes !== 'string' || result.bytes.length > 2_796_204)
          throw problem('invalid')
        const bytes = Uint8Array.from(atob(result.bytes), (character) =>
          character.charCodeAt(0),
        )
        if (bytes.length > 2 * 1024 * 1024) throw problem('invalid')
        return new Blob([bytes], { type: 'image/jpeg' })
      }
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
          throw problem('stale')
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
      void cancelPreview()
      if (selected.peek()?.accountId === accountId) {
        stopRequested = true
        clearEditTimers()
        const kept = { ...drafts.peek() }
        delete kept[selected.peek()!.id]
        drafts.value = kept
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
      const id = selected.peek()?.id
      if (id === undefined || busy.peek()) return
      scheduleEdit(id, { ...drafts.peek()[id], text })
    },
    async editPhotoCaption(file: string, caption: string) {
      const record = selected.peek()
      if (
        !record ||
        busy.peek() ||
        !record.files.some((item) => item.id === file && !item.thumbnail)
      )
        return
      const draft = drafts.peek()[record.id]
      scheduleEdit(record.id, {
        ...draft,
        captions: { ...draft?.captions, [file]: caption },
      })
    },
    async destination(accountId: string, roomId?: string) {
      await flushEdits()
      await change((record) => ({
        ...record,
        accountId: accountId || undefined,
        roomId: roomId || undefined,
        previews:
          record.accountId === accountId
            ? record.previews
            : record.previews?.map((p) => ({
                ...p,
                image_upload_id: undefined,
              })),
      }))
    },
    async removeFile(file: string) {
      if (busy.peek()) return
      await flushEdits()
      await change((record) => record, false, file)
    },
    async removePreview(url: string, imageOnly = false) {
      await flushEdits()
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
      void cancelPreview()
      clearEditTimers()
      drafts.value = {}
      stopRequested = true
      epoch += 1
      selected.value = null
      pending.value = []
      busy.value = false
      error.value = null
      notice.value = null
      unavailable.value = false
      sessionError.value = null
    },
  }
}
