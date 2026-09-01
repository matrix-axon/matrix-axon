import { cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import type { VersionManifest } from '../stores/update-check'
import { testServices } from '../test/services'
import { UpdateBanner } from './UpdateBanner'

afterEach(cleanup)

const NEWER: VersionManifest = {
  release: '0.2.0',
  version: 'build-b',
  builtAt: '2026-08-02T14:03:11.204Z',
}

async function setup(manifest: VersionManifest | null) {
  const services = testServices({
    currentVersion: 'build-a',
    versionManifest: () => Promise.resolve(manifest),
  })
  const utils = render(
    <ServicesContext.Provider value={services}>
      <UpdateBanner />
    </ServicesContext.Provider>,
  )
  await services.updates.check()
  return { services, ...utils }
}

describe('UpdateBanner', () => {
  it('renders nothing while the client is current', async () => {
    const { queryByRole } = await setup({ ...NEWER, version: 'build-a' })
    expect(queryByRole('status')).toBeNull()
  })

  it('announces an available update with its version', async () => {
    const { getByRole } = await setup(NEWER)
    const status = getByRole('status')
    expect(status.textContent).toContain('A new version of Axon is available')
    // release+hash, so a bug report can name the build it was offered.
    expect(status.textContent).toContain('0.2.0+build-b')
  })

  it('reloads when the user asks', async () => {
    const reload = vi.fn()
    vi.spyOn(window, 'location', 'get').mockReturnValue({
      ...window.location,
      reload,
    } as unknown as Location)

    const { getByText } = await setup(NEWER)
    fireEvent.click(getByText('Reload'))
    expect(reload).toHaveBeenCalledTimes(1)
    vi.restoreAllMocks()
  })

  it('can be dismissed for this visit', async () => {
    const { getByLabelText, queryByRole } = await setup(NEWER)
    fireEvent.click(getByLabelText('Dismiss update notice'))
    expect(queryByRole('status')).toBeNull()
  })
})

describe('UpdateBanner in a packaged build', () => {
  it('never offers a reload that cannot bring a new version', async () => {
    // The shell's bundle is its binary, so a reload returns exactly what is
    // already running. Asserted independently of the poller not starting —
    // that is true today and is an accident of wiring, not the reason.
    const services = testServices({
      currentVersion: 'build-a',
      versionManifest: () => Promise.resolve(NEWER),
      platform: { updatesFromOrigin: false },
    })
    const { queryByRole, queryByText } = render(
      <ServicesContext.Provider value={services}>
        <UpdateBanner />
      </ServicesContext.Provider>,
    )
    await services.updates.check()

    // The checker did see a newer build; the banner still must not appear.
    expect(services.updates.available.value).toBe(true)
    expect(queryByText(/A new version of Axon is available/)).toBeNull()
    expect(queryByRole('button', { name: 'Reload' })).toBeNull()
  })
})
