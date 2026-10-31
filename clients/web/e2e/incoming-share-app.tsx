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
const port: IncomingSharePort = {
  subscribe(listener) {
    window.addEventListener('focus', listener)
    return () => window.removeEventListener('focus', listener)
  },
  async call<T>(action: string, args = {}): Promise<T> {
    const records = read()
    if (action === 'list') return records as T
    if (action === 'check_transport') {
      if (localStorage.getItem('fixture.httpSharing') === 'true')
        throw { code: 'httpsRequired' }
      return {} as T
    }
    const record = records.find((item) => item.id === args.id)
    if (action === 'claim' && record !== undefined) {
      record.owner = crypto.randomUUID()
      localStorage.setItem('fixture.incoming', JSON.stringify(records))
      return record as T
    }
    if (action === 'update') {
      const next = args.manifest as IncomingShare
      const index = records.findIndex((item) => item.id === next.id)
      if (
        index < 0 ||
        records[index].revision !== next.revision ||
        records[index].owner !== next.owner
      )
        throw new Error('stale')
      records[index] = {
        ...next,
        revision: next.revision + 1,
        owner: args.release === true ? undefined : next.owner,
      }
      localStorage.setItem('fixture.incoming', JSON.stringify(records))
      return records[index] as T
    }
    if (action === 'ack' || action === 'discard') {
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
