import { expect, test } from '@playwright/test'

test('the mock completes a client-initiated WebSocket close', async ({
  page,
}) => {
  await page.goto('/')
  await page.evaluate(
    () =>
      new Promise<void>((resolve, reject) => {
        const socket = new WebSocket(`ws://${location.host}/v1/ws`, ['axon'])
        socket.onopen = () => {
          socket.close()
          resolve()
        }
        socket.onerror = () => reject(new Error('WebSocket failed to open'))
      }),
  )

  await expect
    .poll(
      async () => {
        const response = await page.request.get('/__e2e/socket-count')
        const body = (await response.json()) as { data: { open: number } }
        return body.data.open
      },
      { timeout: 2000 },
    )
    .toBe(0)
})
