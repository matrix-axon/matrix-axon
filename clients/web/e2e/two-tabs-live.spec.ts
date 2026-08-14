import {
  expect,
  test,
  type BrowserContext,
  type BrowserContextOptions,
  type TestInfo,
} from '@playwright/test'
import { LIVE_TIMEOUT_MS, RECONNECT_TIMEOUT_MS } from './helpers'

const ACCOUNT_ID = '11111111-1111-4111-8111-111111111111'
const ROOM_ID = '!room:hs'
const ROOM_URL = `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`
const SEND_URL = `/v1/accounts/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}/send`

/**
 * `mode: 'serial'` because the mock backend is one process with shared state
 * (its socket set and room history), so dropping sockets in one test would sever
 * another's connection.
 *
 * `timeout` because every test here opens at least one socket on the
 * `LIVE_TIMEOUT_MS` budget and some open two, and the reconnect test adds
 * `RECONNECT_TIMEOUT_MS` on top — up to ~36s against Playwright's 30s per-test
 * default. Without this the *outer* timeout fires first and reports a generic
 * "Test timeout of 30000ms exceeded", throwing away the budget the inner
 * assertions were sized to spend. Set on the file rather than per test so a new
 * socket-opening test here inherits it instead of rediscovering this.
 */
test.describe.configure({ mode: 'serial', timeout: 60_000 })

/** Explicit contexts do not inherit a project's device profile automatically. */
function projectContextOptions(testInfo: TestInfo): BrowserContextOptions {
  const { deviceScaleFactor, hasTouch, isMobile, userAgent, viewport } =
    testInfo.project.use
  return { deviceScaleFactor, hasTouch, isMobile, userAgent, viewport }
}

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
    { timeout: LIVE_TIMEOUT_MS },
  )
  return page
}

test('two tabs see each other messages live', async ({ browser }, testInfo) => {
  const options = projectContextOptions(testInfo)
  const contextA = await browser.newContext(options)
  const contextB = await browser.newContext(options)

  const tabA = await openRoom(contextA)
  const tabB = await openRoom(contextB)

  // Tab A sends; the mock broadcasts a timeline.event to every socket.
  //
  // Enter rather than the Send button, matching the sibling test below. What
  // this file is about is the socket, not which affordance submitted — and the
  // button has its own coverage in `media-send`, `reaction-scroll` and
  // `layout`. Enter was measured working on all three engines, so there is no
  // cross-browser reason to differ here.
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
}, testInfo) => {
  const options = projectContextOptions(testInfo)
  const contextA = await browser.newContext(options)
  const contextB = await browser.newContext(options)

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
}, testInfo) => {
  const context = await browser.newContext(projectContextOptions(testInfo))
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
  // the room head — the only path by which this event can appear. The 1.5s
  // upgrade block guarantees the first attempt is refused, so recovery cannot
  // land before the second rung; the budget clears the fourth plus the gap-fill.
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
    { timeout: RECONNECT_TIMEOUT_MS },
  )
  await expect(page.getByText(missed)).toBeVisible()

  await context.close()
})
