import { expect, test, type APIRequestContext } from '@playwright/test'
import { randomBytes, randomUUID } from 'node:crypto'
import { connect, type Socket } from 'node:net'

/**
 * Where the mock is listening, taken from the project's `baseURL`.
 *
 * This is the only spec that opens a raw TCP socket instead of going through
 * Playwright, so it is the only one that has to name the address at all — the
 * rest get it for free from relative URLs. Hardcoding it would put a second copy
 * of `playwright.config.ts`'s port here, and if the two ever diverged the symptom
 * would be this probe connecting to nothing and reporting an upgrade timeout,
 * which reads as a broken mock rather than as a stale constant.
 */
function mockAddress(): { host: string; port: number } {
  const baseURL = test.info().project.use.baseURL
  if (baseURL === undefined) {
    throw new Error(
      'no baseURL on this project to derive the mock address from',
    )
  }
  const { hostname, port } = new URL(baseURL)
  return { host: hostname, port: Number(port) }
}

function openRawWebSocket(probe: string): Promise<Socket> {
  const { host, port } = mockAddress()
  return new Promise((resolve, reject) => {
    const socket = connect({ host, port }, () => {
      socket.write(
        [
          `GET /v1/ws?probe=${encodeURIComponent(probe)} HTTP/1.1`,
          `Host: ${host}:${port}`,
          'Upgrade: websocket',
          'Connection: Upgrade',
          `Sec-WebSocket-Key: ${randomBytes(16).toString('base64')}`,
          'Sec-WebSocket-Version: 13',
          'Sec-WebSocket-Protocol: axon',
          '',
          '',
        ].join('\r\n'),
      )
    })
    socket.setTimeout(2000, () => {
      reject(new Error('Timed out waiting for the WebSocket upgrade'))
      socket.destroy()
    })
    let response = Buffer.alloc(0)
    function onData(data: Buffer): void {
      response = Buffer.concat([response, data])
      if (!response.includes('\r\n\r\n')) return
      socket.off('data', onData)
      if (!response.toString().startsWith('HTTP/1.1 101 ')) {
        reject(new Error(`WebSocket upgrade failed: ${response.toString()}`))
        socket.destroy()
        return
      }
      socket.setTimeout(0)
      resolve(socket)
    }
    socket.on('data', onData)
    socket.once('error', reject)
  })
}

function emptyMaskedControlFrame(opcode: number): Buffer {
  return Buffer.concat([Buffer.from([0x80 | opcode, 0x80]), randomBytes(4)])
}

async function expectProbeClosed(
  request: APIRequestContext,
  probe: string,
): Promise<void> {
  await expect
    .poll(
      async () => {
        const response = await request.get(
          `/__e2e/socket-open?probe=${encodeURIComponent(probe)}`,
        )
        const body = (await response.json()) as { data: { open: boolean } }
        return body.data.open
      },
      { timeout: 2000 },
    )
    .toBe(false)
}

test('the mock completes a client-initiated WebSocket close', async ({
  page,
}) => {
  const probe = randomUUID()
  await page.goto('/')
  await page.evaluate(
    (probeId) =>
      new Promise<void>((resolve, reject) => {
        const socket = new WebSocket(
          `ws://${location.host}/v1/ws?probe=${encodeURIComponent(probeId)}`,
          ['axon'],
        )
        socket.onopen = () => {
          socket.close()
          resolve()
        }
        socket.onerror = () => reject(new Error('WebSocket failed to open'))
      }),
    probe,
  )

  await expectProbeClosed(page.request, probe)
})

test('the mock reassembles a Close frame split across TCP chunks', async ({
  request,
}) => {
  const probe = randomUUID()
  const socket = await openRawWebSocket(probe)
  try {
    const ping = emptyMaskedControlFrame(0x09)
    const close = emptyMaskedControlFrame(0x08)

    // Leave the Close frame's opcode byte behind a complete Ping, then split
    // the remainder into a later TCP write. A data-event-is-a-frame parser
    // consumes the Ping and loses the Close boundary in exactly this shape.
    socket.write(Buffer.concat([ping, close.subarray(0, 1)]))
    await new Promise((resolve) => setTimeout(resolve, 20))
    socket.write(close.subarray(1))

    await expectProbeClosed(request, probe)
  } finally {
    socket.destroy()
  }
})
