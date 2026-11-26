import { apiErrorMessage, type ApiClient } from '../api/client'
import type {
  DiscoveredMembership,
  SpaceChildDto,
  SpaceGraph,
  SpaceGraphLink,
} from './space-graph'

type SpacePage<Row> = {
  rows: readonly Row[]
  next_cursor?: string | null
}

async function readPages<Row>(
  read: (cursor?: string) => Promise<SpacePage<Row>>,
): Promise<Row[]> {
  const rows: Row[] = []
  const cursors = new Set<string>()
  let cursor: string | undefined
  do {
    const page = await read(cursor)
    for (const row of page.rows) rows.push(row)
    cursor = page.next_cursor ?? undefined
    if (cursor === undefined) break
    if (cursors.has(cursor)) throw new Error('Repeated space page cursor')
    cursors.add(cursor)
  } while (cursor !== undefined)
  return rows
}

function unsupported(response: Response): boolean {
  return response.status === 404 || response.status === 405
}

export async function readSpaceGraph(
  api: ApiClient,
  accountId: string,
  spaces: ReadonlyMap<string, string>,
  signal: AbortSignal,
): Promise<SpaceGraph> {
  const path = { account_id: accountId }
  const first = await api.GET('/v1/accounts/{account_id}/spaces/graph', {
    params: { path, query: { limit: 1000 } },
    signal,
  })
  if (unsupported(first.response)) {
    return readLegacyChildren(api, accountId, spaces, signal)
  }
  const links = await readPages<SpaceGraphLink>(async (cursor) => {
    const { data, error } =
      cursor === undefined
        ? first
        : await api.GET('/v1/accounts/{account_id}/spaces/graph', {
            params: { path, query: { cursor, limit: 1000 } },
            signal,
          })
    if (data === undefined) throw new Error(apiErrorMessage(error))
    return { rows: data.data.links, next_cursor: data.data.next_cursor }
  })
  return { links }
}

export function readDiscoveredMemberships(
  api: ApiClient,
  accountId: string,
  signal: AbortSignal,
): Promise<DiscoveredMembership[]> {
  return readPages(async (cursor) => {
    const { data, error, response } = await api.GET(
      '/v1/accounts/{account_id}/spaces/discovered',
      {
        params: {
          path: { account_id: accountId },
          query: { cursor, limit: 100 },
        },
        signal,
      },
    )
    if (cursor === undefined && unsupported(response)) return { rows: [] }
    if (data === undefined) throw new Error(apiErrorMessage(error))
    return {
      rows: data.data.memberships,
      next_cursor: data.data.next_cursor,
    }
  })
}

async function readLegacyChildren(
  api: ApiClient,
  accountId: string,
  spaces: ReadonlyMap<string, string>,
  signal: AbortSignal,
): Promise<SpaceGraph> {
  const legacy = new Map<string, readonly SpaceChildDto[]>()
  const errors = new Map<string, string>()
  // Older servers have only a per-space read. Bound their request fan-out.
  const entries = [...spaces]
  for (let start = 0; start < entries.length; start += 2) {
    signal.throwIfAborted()
    await Promise.all(
      entries.slice(start, start + 2).map(async ([key, roomId]) => {
        try {
          const { data, error } = await api.GET(
            '/v1/accounts/{account_id}/rooms/{room_id}/space/children',
            {
              params: { path: { account_id: accountId, room_id: roomId } },
              signal,
            },
          )
          if (data === undefined) throw new Error(apiErrorMessage(error))
          legacy.set(
            key,
            data.data.filter((child) => child.via.length > 0),
          )
        } catch (error) {
          errors.set(key, spaceLoadError(error))
        }
      }),
    )
  }
  return { legacy, errors }
}

export function spaceLoadError(error: unknown): string {
  return error instanceof Error
    ? `Could not load space: ${error.message}`
    : 'Could not load space.'
}
