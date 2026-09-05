import { cleanup, fireEvent, render, screen } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { testServices } from '../test/services'
import { SasModal } from './SasModal'

const ACCOUNT = '11111111-1111-1111-1111-111111111111'

const seven = [
  { symbol: '🐶', description: 'Dog' },
  { symbol: '🐱', description: 'Cat' },
  { symbol: '🦁', description: 'Lion' },
  { symbol: '🐴', description: 'Horse' },
  { symbol: '🦄', description: 'Unicorn' },
  { symbol: '🐷', description: 'Pig' },
  { symbol: '🐘', description: 'Elephant' },
]

afterEach(() => cleanup())

describe('SasModal', () => {
  it('enables They-match only with seven emoji and parks on Close', async () => {
    const services = testServices()
    services.verification.noteFrame(ACCOUNT, 'requested', {
      flowId: '$f',
      userId: '@me:hs',
      deviceId: 'DEV',
      emoji: null,
      decimals: null,
      reason: null,
    })
    services.verification.noteFrame(ACCOUNT, 'sas', {
      flowId: '$f',
      userId: '@me:hs',
      deviceId: 'DEV',
      emoji: [{ symbol: '🐶', description: 'Dog' }],
      decimals: null,
      reason: null,
    })
    services.verification.open(`${ACCOUNT}\0$f`)
    const first = render(
      <ServicesContext.Provider value={services}>
        <SasModal />
      </ServicesContext.Provider>,
    )
    expect(
      await screen.findByText('Waiting for a complete emoji set'),
    ).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'They match' })).toBeNull()
    first.unmount()

    services.verification.noteFrame(ACCOUNT, 'sas', {
      flowId: '$f',
      userId: '@me:hs',
      deviceId: 'DEV',
      emoji: seven,
      decimals: [1, 2, 3],
      reason: null,
    })
    render(
      <ServicesContext.Provider value={services}>
        <SasModal />
      </ServicesContext.Provider>,
    )
    expect(await screen.findByText('Dog')).toBeTruthy()
    expect(
      (screen.getByRole('button', { name: 'They match' }) as HTMLButtonElement)
        .disabled,
    ).toBe(false)
    fireEvent.click(screen.getByRole('button', { name: 'Close' }))
    expect(services.verification.openFlow.value).toBeNull()
    expect(services.verification.inboxCount.value).toBe(1)
  })

  it('Escape parks a live flow and keeps it in the inbox', async () => {
    const services = testServices()
    services.verification.noteFrame(ACCOUNT, 'requested', {
      flowId: '$f',
      userId: '@me:hs',
      deviceId: 'DEV',
      emoji: null,
      decimals: null,
      reason: null,
    })
    services.verification.open(`${ACCOUNT}\0$f`)
    render(
      <ServicesContext.Provider value={services}>
        <SasModal />
      </ServicesContext.Provider>,
    )
    expect(await screen.findByRole('dialog')).toBeTruthy()
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(services.verification.openFlow.value).toBeNull()
    expect(services.verification.inboxCount.value).toBe(1)
  })
})
