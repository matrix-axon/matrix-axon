import { cleanup, render, waitFor } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { createLiveConnection } from '../stores/live-connection'
import { FakeWebSocket } from '../test/fake-socket'
import { testServices } from '../test/services'
import { ConnectionIndicator } from './ConnectionIndicator'

afterEach(cleanup)

function setup() {
  const socket = new FakeWebSocket()
  const live = createLiveConnection({
    socketFactory: () => socket.asWebSocket(),
  })
  const services = { ...testServices(), live }
  const utils = render(
    <ServicesContext.Provider value={services}>
      <ConnectionIndicator />
    </ServicesContext.Provider>,
  )
  return { socket, live, ...utils }
}

describe('ConnectionIndicator', () => {
  it('reflects each connection state with label and class', async () => {
    const { socket, live, getByRole } = setup()
    const status = getByRole('status')

    expect(status.textContent).toBe('Offline')
    expect(status.className).toContain('conn-offline')
    expect(status.getAttribute('title')).toBe('WebSocket: Offline')

    live.start()
    await waitFor(() => expect(status.textContent).toBe('Connecting…'))
    expect(status.className).toContain('conn-connecting')

    socket.emitOpen()
    await waitFor(() => expect(status.textContent).toBe('Live'))
    expect(status.className).toContain('conn-live')
    expect(status.getAttribute('title')).toBe('WebSocket: Live')

    socket.emitClose()
    await waitFor(() => expect(status.textContent).toBe('Reconnecting…'))
    expect(status.className).toContain('conn-reconnecting')
  })
})
