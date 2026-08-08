import { expect, test, type BrowserContext } from '@playwright/test'

const ACCOUNT_ID = '11111111-1111-4111-8111-111111111111'
const ROOM_ID = '!room:hs'
const ROOM_URL = `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`
const SEND_URL = `/v1/accounts/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}/send`

// The mock backend is one process with shared state (its socket set and room
// history), so dropping sockets in one test would sever another's connection.
test.describe.configure({ mode: 'serial' })

/** A signed-in tab: seed the token before app scripts run, then open the room. */
async function openRoom(context: BrowserContext) {
  await context.addInitScript(() =>
    localStorage.setItem('axon.token', 'e2e-token'),
  )
  const page = await context.newPage()
  await page.goto(ROOM_URL)
  // The connection indicator reaching "Live" proves the #238 handshake and the
  // LiveConnection wiring against a real socket.
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  return page
}

test('two tabs see each other messages live', async ({ browser }) => {
  const contextA = await browser.newContext()
  const contextB = await browser.newContext()

  const tabA = await openRoom(contextA)
  const tabB = await openRoom(contextB)

  // Tab A sends; the mock broadcasts a timeline.event to every socket.
  const message = `live hello ${Date.now()}`
  const composer = tabA.getByRole('textbox', { name: /^Message/ })
  await composer.fill(message)
  await composer.press('Enter')

  // Tab B renders it live — no reload — via the M-W6 frame router + ingestLive.
  await expect(
    tabB.locator('.event-row .body-text', { hasText: message }),
  ).toBeVisible()
  // And the sender sees it too (local echo reconciled with the live frame).
  await expect(
    tabA.locator('.event-row .body-text', { hasText: message }),
  ).toBeVisible()

  await contextA.close()
  await contextB.close()
})

test('typing in one tab surfaces the indicator in another', async ({
  browser,
}) => {
  const contextA = await browser.newContext()
  const contextB = await browser.newContext()

  const tabA = await openRoom(contextA)
  const tabB = await openRoom(contextB)

  // A real keystroke in tab A drives onDraftChange → the outbound typing
  // notice (ADR 0068 M19a). The mock echoes it as a peer's `m.typing`
  // passthrough, which tab B renders live.
  const composer = tabA.getByRole('textbox', { name: /^Message/ })
  await composer.fill('drafting a reply')
  await expect(tabB.getByText(/is typing/)).toBeVisible()

  // Sending clears the notice (typing:false), so the indicator disappears.
  await composer.press('Enter')
  await expect(tabB.getByText(/is typing/)).toBeHidden()

  await contextA.close()
  await contextB.close()
})

test('a dropped socket shows Reconnecting, then heals by gap-fill', async ({
  browser,
  request,
}) => {
  const context = await browser.newContext()
  const page = await openRoom(context)

  // Kill the socket rudely (no close frame) and refuse upgrades for a moment,
  // so the client cannot reconnect in time to receive what we send next.
  await request.post('/__e2e/drop-sockets?block_ms=1500')
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    /Reconnecting/,
  )

  // Sent while the tab is disconnected: the broadcast reaches nobody, so this
  // event exists only in the room history. The bus is lossy by design.
  const missed = `missed while offline ${Date.now()}`
  await request.post(SEND_URL, { data: { body: missed } })
  await expect(page.getByText(missed)).toBeHidden()

  // Backoff reconnects once upgrades are allowed again, and gap-fill refetches
  // the room head — the only path by which this event can appear.
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await expect(page.getByText(missed)).toBeVisible()

  await context.close()
})
