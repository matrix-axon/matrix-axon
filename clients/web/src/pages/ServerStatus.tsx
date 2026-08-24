import { useEffect, useState } from 'preact/hooks'
import { apiErrorMessage } from '../api/client'
import type { components } from '../api/schema'
import { CopyableText } from '../components/CopyableText'
import { useServices } from '../services'

type StatusDto = components['schemas']['StatusDto']
export type ServerBuildInfo = {
  build_time: string
  git_hash: string
  profile: string
  rustc_version: string
  version: string
}
type AccountSyncStatus = {
  account_id: string
  since_ms: number
  state: string
}
type ServerStatusDto = StatusDto & {
  build?: ServerBuildInfo
  sync?: AccountSyncStatus[]
}

/**
 * Server status (`GET /v1/status`): the backfill engine's disk-space health
 * and per-account progress — the "status" leg of the M-W3 lifecycle scope.
 */
export function ServerStatus() {
  const { api } = useServices()
  const [status, setStatus] = useState<ServerStatusDto | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    void api.GET('/v1/status').then(
      ({ data, error: apiError }) => {
        if (cancelled) {
          return
        }
        if (apiError !== undefined) {
          setError(apiErrorMessage(apiError))
        } else {
          setStatus(data.data)
        }
      },
      (cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof Error ? cause.message : String(cause))
        }
      },
    )
    return () => {
      cancelled = true
    }
  }, [api])

  if (error !== null) {
    return <p class="muted">Server status unavailable: {error}</p>
  }
  if (status === null) {
    return null
  }

  const backfill = status.backfill
  const build = status.build
  const sync = status.sync ?? []
  const gib = (backfill.free_bytes / 1024 ** 3).toFixed(1)
  return (
    <section class="panel">
      <h2>Server status</h2>
      {build !== undefined && (
        <p>
          <CopyableText
            text={formatServerBuildLine(build)}
            label="server status"
          >
            Axon server <code>{build.version}</code>
            {build.git_hash !== '' && (
              <>
                {' '}
                · <code>{shortHash(build.git_hash)}</code>
              </>
            )}
            {' · '}
            {build.profile}
            {' · '}
            <time dateTime={build.build_time}>
              {formatDate(build.build_time)}
            </time>
            {' · '}
            <code>{build.rustc_version}</code>
          </CopyableText>
        </p>
      )}
      <p>
        History backfill:{' '}
        {backfill.paused ? (
          <strong class="warn">
            paused ({backfill.reason ?? 'unknown reason'})
          </strong>
        ) : (
          'running'
        )}{' '}
        · {gib} GiB free
      </p>
      {backfill.accounts.length > 0 && (
        <ul class="status-list">
          {backfill.accounts.map((account) => (
            <li key={account.account_id}>
              <code>{account.account_id.slice(0, 8)}</code>:{' '}
              {account.rooms_backfilled}/{account.rooms_total} rooms backfilled,{' '}
              {account.events} events
              {account.complete ? ' — complete' : ''}
            </li>
          ))}
        </ul>
      )}
      {sync.length > 0 && (
        <>
          <h3>Sync service</h3>
          <ul class="status-list">
            {sync.map((account) => (
              <li key={account.account_id}>
                <code>{account.account_id.slice(0, 8)}</code>:{' '}
                <span class={`badge sync-${account.state}`}>
                  {account.state}
                </span>{' '}
                since{' '}
                <time dateTime={new Date(account.since_ms).toISOString()}>
                  {formatDate(account.since_ms)}
                </time>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  )
}

function shortHash(hash: string): string {
  return hash.length > 12 ? hash.slice(0, 12) : hash
}

/** Plain-text form of the build line, matching what the status panel shows. */
export function formatServerBuildLine(build: ServerBuildInfo): string {
  const parts = [`Axon server ${build.version}`]
  if (build.git_hash !== '') {
    parts.push(shortHash(build.git_hash))
  }
  parts.push(build.profile, formatDate(build.build_time), build.rustc_version)
  return parts.join(' · ')
}

function formatDate(value: string | number): string {
  return new Date(value).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  })
}
