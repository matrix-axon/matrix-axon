import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { afterEach, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import type { RoomEntryResult } from '../stores/rooms'
import { testServices } from '../test/services'
import { useStartDm } from './use-start-dm'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

function Harness({ onStarted }: { onStarted: (roomId: string) => void }) {
  const { startDm, pendingUserId, error, invalidate } = useStartDm(
    ACCOUNT,
    onStarted,
  )
  return (
    <>
      <button type="button" onClick={() => void startDm('@a:hs')}>
        start-a
      </button>
      <button type="button" onClick={() => void startDm('@b:hs')}>
        start-b
      </button>
      <button type="button" onClick={invalidate}>
        invalidate
      </button>
      <span data-testid="pending">{pendingUserId ?? ''}</span>
      <span data-testid="error">{error ?? ''}</span>
    </>
  )
}

afterEach(() => {
  cleanup()
})

it('reports a thrown direct-message start and clears its pending state', async () => {
  const services = testServices()
  vi.spyOn(services.rooms, 'createDm').mockRejectedValueOnce(
    new Error('socket down'),
  )
  const onStarted = vi.fn()
  const view = render(
    <ServicesContext.Provider value={services}>
      <Harness onStarted={onStarted} />
    </ServicesContext.Provider>,
  )
  fireEvent.click(view.getByRole('button', { name: 'start-a' }))
  await waitFor(() =>
    expect(view.getByTestId('error').textContent).toBe(
      'Could not start DM with @a:hs: socket down',
    ),
  )
  expect(view.getByTestId('pending').textContent).toBe('')
  expect(onStarted).not.toHaveBeenCalled()
})

it('does not clear a newer direct-message start when an older one throws', async () => {
  const services = testServices()
  let rejectFirst: (reason: unknown) => void = () => {}
  let resolveSecond: (value: RoomEntryResult) => void = () => {}
  const first = new Promise<RoomEntryResult>((_resolve, reject) => {
    rejectFirst = reject
  })
  const second = new Promise<RoomEntryResult>((resolve) => {
    resolveSecond = resolve
  })
  const createDm = vi.spyOn(services.rooms, 'createDm')
  createDm.mockImplementationOnce(() => first)
  createDm.mockImplementationOnce(() => second)
  const onStarted = vi.fn()
  const view = render(
    <ServicesContext.Provider value={services}>
      <Harness onStarted={onStarted} />
    </ServicesContext.Provider>,
  )
  fireEvent.click(view.getByRole('button', { name: 'start-a' }))
  expect(view.getByTestId('pending').textContent).toBe('@a:hs')
  fireEvent.click(view.getByRole('button', { name: 'invalidate' }))
  expect(view.getByTestId('pending').textContent).toBe('')
  fireEvent.click(view.getByRole('button', { name: 'start-b' }))
  expect(view.getByTestId('pending').textContent).toBe('@b:hs')

  rejectFirst(new Error('socket down'))
  await first.catch(() => undefined)
  await waitFor(() =>
    expect(view.getByTestId('pending').textContent).toBe('@b:hs'),
  )
  expect(view.getByTestId('error').textContent).toBe('')
  expect(onStarted).not.toHaveBeenCalled()

  resolveSecond({ ok: true, roomId: '!created:hs' })
  await waitFor(() => expect(onStarted).toHaveBeenCalledWith('!created:hs'))
  expect(view.getByTestId('pending').textContent).toBe('')
  expect(view.getByTestId('error').textContent).toBe('')
})
