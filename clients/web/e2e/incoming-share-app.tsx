// Separate build entry, emitted ONLY with AXON_SHARE_E2E=1. Exercises the real
// app and recovery store; only the native file/ownership bridge is substituted.
import { render } from 'preact'
import { App } from '../src/app'
import { browserPlatform } from '../src/platform'
import type {
  IncomingShare,
  IncomingSharePort,
} from '../src/incoming-share/types'
import '../src/index.css'

const read = (): IncomingShare[] =>
  JSON.parse(
    localStorage.getItem('fixture.incoming') ?? '[]',
  ) as IncomingShare[]
const previews = new Map<string, () => void>()
const port: IncomingSharePort = {
  subscribe(listener) {
    window.addEventListener('focus', listener)
    return () => window.removeEventListener('focus', listener)
  },
  async call<T>(action: string, args = {}): Promise<T> {
    const records = read()
    if (action === 'cancel_preview') {
      previews.get(args.requestId as string)?.()
      return {} as T
    }
    if (action === 'list') {
      if (localStorage.getItem('fixture.inboxUnavailable') === 'true')
        throw { code: 'unavailable' }
      return records as T
    }
    if (action === 'check_transport') {
      if (localStorage.getItem('fixture.httpSharing') === 'true')
        throw { code: 'httpsRequired' }
      return {} as T
    }
    const record = records.find((item) => item.id === args.id)
    if (action === 'device_preview' && record !== undefined) {
      if (localStorage.getItem('fixture.waitForPreview') === 'true') {
        const id = args.requestId as string
        try {
          await new Promise<void>((_, reject) =>
            previews.set(id, () => reject({ code: 'cancelled' })),
          )
        } finally {
          previews.delete(id)
        }
      }
      if (localStorage.getItem('fixture.devicePreviewFailure') === 'true')
        throw { code: 'devicePreview_timeout' }
      record.previews = [
        {
          matched_url: args.url as string,
          title: 'Generated on device',
          provenance: 'generated',
        },
      ]
      record.revision += 1
      localStorage.setItem('fixture.incoming', JSON.stringify(records))
      return record as T
    }
    if (action === 'claim' && record !== undefined) {
      record.owner = crypto.randomUUID()
      localStorage.setItem('fixture.incoming', JSON.stringify(records))
      return record as T
    }
    if (action === 'photo_thumbnail') {
      // Native returns a bounded JPEG. Generate an independently encoded,
      // valid fixture instead of relying on a CRC-invalid sample PNG.
      const canvas = document.createElement('canvas')
      canvas.width = canvas.height = 16
      const context = canvas.getContext('2d')!
      context.fillStyle = '#0088ff'
      context.fillRect(0, 0, 16, 16)
      return { bytes: canvas.toDataURL('image/jpeg').split(',')[1] } as T
    }
    if (action === 'update' || action === 'remove_file') {
      const next = args.manifest as IncomingShare
      const index = records.findIndex((item) => item.id === next.id)
      if (
        index < 0 ||
        records[index].revision !== next.revision ||
        records[index].owner !== next.owner
      )
        throw new Error('stale')
      if (action === 'remove_file')
        next.files = next.files.filter((file) => file.id !== args.file)
      records[index] = {
        ...next,
        revision: next.revision + 1,
        owner: args.release === true ? undefined : next.owner,
      }
      localStorage.setItem('fixture.incoming', JSON.stringify(records))
      return records[index] as T
    }
    if (action === 'ack' || action === 'discard') {
      if (args.owner !== undefined && record?.owner !== args.owner)
        throw { code: 'stale' }
      if (
        action === 'discard' &&
        record?.owner &&
        record.operation.state === 'dispatching'
      )
        throw { code: 'sending' }
      localStorage.setItem(
        'fixture.incoming',
        JSON.stringify(records.filter((item) => item.id !== args.id)),
      )
      return {} as T
    }
    throw new Error('unsupported fixture operation')
  },
}
history.replaceState(null, '', '/pending-shares')
render(
  <App platform={{ ...browserPlatform(), incomingShare: port }} />,
  document.getElementById('app')!,
)
