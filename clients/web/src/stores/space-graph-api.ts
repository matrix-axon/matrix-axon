import { apiErrorMessage, type ApiClient } from '../api/client'
import type {
  DiscoveredMembership,
  SpaceChildDto,
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
  signal: AbortSignal,
): Promise<SpaceGraphLink[] | null> {
  const path = { account_id: accountId }
  const first = await api.GET('/v1/accounts/{account_id}/spaces/graph', {
    params: { path, query: { limit: 1000 } },
    signal,
  })
  if (unsupported(first.response)) return null
  return readPages<SpaceGraphLink>(async (cursor) => {
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

export async function readLegacyChildren(
  api: ApiClient,
  accountId: string,
  roomId: string,
  signal: AbortSignal,
): Promise<SpaceChildDto[]> {
  const { data, error } = await api.GET(
    '/v1/accounts/{account_id}/rooms/{room_id}/space/children',
    {
      params: { path: { account_id: accountId, room_id: roomId } },
      signal,
    },
  )
  if (data === undefined) throw new Error(apiErrorMessage(error))
  return data.data.filter((child) => child.via.length > 0)
}

export function spaceLoadError(error: unknown): string {
  return error instanceof Error
    ? `Could not load space: ${error.message}`
    : 'Could not load space.'
}
