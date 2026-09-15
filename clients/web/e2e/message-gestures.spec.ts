import { expect, test, type Locator, type Page } from '@playwright/test'
import { expectLive, ROOM_URL, signIn } from './helpers'

test.describe.configure({ mode: 'serial' })

test.beforeEach(async ({ request }) => {
  await request.post('/__e2e/reset-message-gestures')
})

test.afterEach(async ({ request }) => {
  await request.post('/__e2e/reset-message-gestures')
})

async function touchPointer(
  target: Locator,
  type: 'pointerdown' | 'pointermove' | 'pointerup',
  x: number,
  y: number,
): Promise<void> {
  await target.dispatchEvent(type, {
    bubbles: true,
    cancelable: true,
    clientX: x,
    clientY: y,
    isPrimary: true,
    pointerId: 1,
    pointerType: 'touch',
  })
}

async function tap(target: Locator): Promise<void> {
  await touchPointer(target, 'pointerdown', 200, 300)
  await touchPointer(target, 'pointerup', 200, 300)
}

async function hold(target: Locator): Promise<void> {
  await touchPointer(target, 'pointerdown', 200, 300)
  await target.page().waitForTimeout(650)
  await touchPointer(target, 'pointerup', 200, 300)
}

async function swipeLeft(
  target: Locator,
  onPreview?: () => Promise<void>,
): Promise<void> {
  await touchPointer(target, 'pointerdown', 260, 300)
  await touchPointer(target, 'pointermove', 210, 302)
  await onPreview?.()
  await touchPointer(target, 'pointermove', 140, 304)
  await touchPointer(target, 'pointerup', 140, 304)
}

async function roomTouch(
  target: Locator,
  type: 'touchstart' | 'touchmove' | 'touchend',
  x: number,
  y: number,
): Promise<void> {
  await target.evaluate(
    (element, point) => {
      const touch = {
        identifier: 1,
        target: element,
        clientX: point.x,
        clientY: point.y,
      }
      // WebKit exposes `Touch` but does not allow constructing it. Defining
      // the read-only lists on a real cancelable event supplies exactly the
      // stream Preact's delegated touch handler reads.
      const event = new Event(point.type, { bubbles: true, cancelable: true })
      const activeTouches = point.type === 'touchend' ? [] : [touch]
      Object.defineProperties(event, {
        touches: { value: activeTouches },
        targetTouches: { value: activeTouches },
        changedTouches: { value: [touch] },
      })
      element.dispatchEvent(event)
    },
    { type, x, y },
  )
}

/**
 * Dispatch the touch stream a pane-level recognizer receives on iOS.
 * Pointer events drive message gestures; this separate stream proves a swipe
 * beginning on a message still bubbles to the existing room navigation.
 */
async function swipeRight(
  target: Locator,
  onPreview?: () => Promise<void>,
): Promise<void> {
  await roomTouch(target, 'touchstart', 90, 300)
  await roomTouch(target, 'touchmove', 125, 302)
  await onPreview?.()
  await roomTouch(target, 'touchmove', 140, 304)
  await roomTouch(target, 'touchend', 140, 304)
}

async function openMobileRoom(page: Page): Promise<void> {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expectLive(page)
}

async function sendMessage(page: Page, body: string): Promise<Locator> {
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.fill(body)
  await page.getByRole('button', { name: 'Send' }).click()
  const row = page.locator('.event-row').filter({ hasText: body }).last()
  await expect(row).toBeVisible()
  await expect(row).toHaveClass(/touch-hold-enabled/)
  return row
}

test('settings autosave the Axon-wide bindings and restore them after reload', async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name !== 'webkit-iphone',
    'gesture settings require the opt-in iPhone WebKit project',
  )
  await signIn(page)
  await page.goto('/settings')

  const bindingRow = page.locator('.message-gesture-bindings label').first()
  const iphoneViewport = page.viewportSize()
  expect(iphoneViewport).not.toBeNull()
  await expect
    .poll(() =>
      bindingRow.evaluate(
        (row) => getComputedStyle(row).gridTemplateColumns.split(' ').length,
      ),
    )
    .toBe(2)
  await page.setViewportSize({ width: 320, height: iphoneViewport!.height })
  await expect
    .poll(() =>
      bindingRow.evaluate(
        (row) => getComputedStyle(row).gridTemplateColumns.split(' ').length,
      ),
    )
    .toBe(1)

  await expect(page.getByLabel('Double tap')).toHaveValue('react')
  await expect(page.getByLabel('Touch and hold')).toHaveValue('thread')
  await expect(page.getByLabel('Swipe left')).toHaveValue('reply')
  await expect(page.getByText('Go back', { exact: true })).toBeVisible()
  await expect(
    page.getByText(/Choosing an action already used by another gesture/),
  ).toBeVisible()

  await page.getByRole('button', { name: 'About gestures' }).click()
  const gestureHelp = page.getByRole('note', { name: 'Gesture help' })
  await expect(gestureHelp).toBeVisible()
  await expect(gestureHelp).toContainText(
    'Gesture settings sync across clients',
  )
  const gestureHelpBox = await gestureHelp.boundingBox()
  const gestureSettingsBox = await page
    .locator('.message-gesture-settings')
    .boundingBox()
  expect(gestureHelpBox).not.toBeNull()
  expect(gestureSettingsBox).not.toBeNull()
  expect(gestureHelpBox!.x).toBeGreaterThanOrEqual(0)
  expect(gestureHelpBox!.x + gestureHelpBox!.width).toBeLessThanOrEqual(320)
  expect(gestureHelpBox!.x).toBeGreaterThanOrEqual(gestureSettingsBox!.x)
  expect(gestureHelpBox!.x + gestureHelpBox!.width).toBeLessThanOrEqual(
    gestureSettingsBox!.x + gestureSettingsBox!.width,
  )
  await gestureHelp.getByRole('button', { name: 'Close' }).click()
  await expect(gestureHelp).not.toBeVisible()
  await page.setViewportSize(iphoneViewport!)

  await page.getByLabel('Double tap').selectOption('thread')
  await expect(page.getByLabel('Touch and hold')).toHaveValue('react')
  await expect(page.getByText('Touch and hold changed to React')).toBeVisible()
  await page.getByLabel('Double tap').selectOption('edit')
  await page.getByLabel('Touch and hold').selectOption('')
  await page
    .getByRole('button', { name: 'Choose reaction emoji, currently 👍' })
    .click()
  await page
    .getByRole('group', { name: 'Choose gesture reaction' })
    .getByRole('button')
    .filter({ hasText: '🎉' })
    .click()
  await expect(page.getByText('Gestures saved')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Save gestures' })).toHaveCount(
    0,
  )

  await page.reload()
  await expect(page.getByLabel('Double tap')).toHaveValue('edit')
  await expect(page.getByLabel('Touch and hold')).toHaveValue('')
  await expect(page.getByLabel('Swipe left')).toHaveValue('reply')
  await expect(
    page.getByRole('button', {
      name: 'Choose reaction emoji, currently 🎉',
    }),
  ).toBeVisible()
  await expect(
    page.getByText(/Native text selection and link previews/),
  ).toBeVisible()

  const settingsPane = page.locator('.settings-back-pane')
  const foregroundPane = page.locator('.shell-body > main')
  const roomList = page.locator('#room-sidebar')
  await expect(roomList).not.toBeVisible()
  await swipeRight(settingsPane, async () => {
    await expect(roomList).toBeVisible()
    await expect(roomList).toContainText('E2E Room')
    await expect(roomList).toHaveClass(/mobile-back-destination-active/)
    await expect(foregroundPane).toHaveCSS(
      'transform',
      /matrix\(1, 0, 0, 1, 35, 0\)/,
    )
  })
  await expect(page).toHaveURL('/')
  await expect(page.getByRole('navigation', { name: 'Rooms' })).toBeVisible()
})

test('default message gestures arbitrate with preserved swipe-right navigation', async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name !== 'webkit-iphone',
    'message gestures require the opt-in iPhone WebKit project',
  )
  await openMobileRoom(page)
  const body = `gesture target ${Date.now()}`
  let row = await sendMessage(page, body)
  let target = row.locator('.event-body')

  await tap(target)
  await tap(target)
  await expect(row.locator('.message-reaction-burst')).toHaveText('👍')
  await expect(row.locator('.message-reaction-burst')).toBeVisible()
  await expect(row.locator('.reaction-chip')).toContainText('👍')
  await expect(row).not.toHaveClass(/actions-open/)

  await hold(target)
  const threadPanel = page.getByRole('complementary', { name: 'Thread' })
  await expect(threadPanel).toBeVisible()
  const timelinePane = page.locator('.room-stream')

  await swipeRight(threadPanel, async () => {
    await expect(timelinePane).toHaveClass(/mobile-back-destination-active/)
    expect(await timelinePane.evaluate((element) => element.inert)).toBe(true)
    await expect(threadPanel).toHaveCSS(
      'transform',
      /matrix\(1, 0, 0, 1, 35, 0\)/,
    )
  })
  await expect(threadPanel).not.toBeVisible()
  await expect(timelinePane).not.toHaveClass(/mobile-back-destination-active/)
  expect(await timelinePane.evaluate((element) => element.inert)).toBe(false)

  row = page.locator('.event-row').filter({ hasText: body }).last()
  target = row.locator('.event-body')
  await swipeLeft(target, async () => {
    await expect(row.locator('.event-content')).toHaveCSS(
      'transform',
      /matrix\(1, 0, 0, 1, -50, 0\)/,
    )
    await expect(row.locator('.gesture-swipe-affordance')).toContainText(
      'Reply',
    )
  })
  await expect(page.getByText('Replying to')).toBeVisible()
  await expect(row.locator('.gesture-swipe-affordance')).toHaveText('Reply')

  await page.goto(ROOM_URL)
  await expectLive(page)
  row = page.locator('.event-row').filter({ hasText: body }).last()
  const roomList = page.locator('#room-sidebar')
  const foregroundPane = page.locator('.shell-body > main')
  target = row.locator('.event-body')

  await roomTouch(target, 'touchstart', 90, 300)
  await roomTouch(target, 'touchmove', 120, 302)
  await expect(roomList).toBeVisible()
  await roomTouch(target, 'touchend', 120, 302)
  expect(await page.evaluate(() => window.location.pathname)).toContain(
    '/rooms/',
  )
  await expect(roomList).not.toBeVisible()
  await expect(foregroundPane).toHaveCSS('transform', 'none')

  await swipeRight(target, async () => {
    await expect(roomList).toBeVisible()
    await expect(roomList).toContainText('E2E Room')
    await expect(roomList).toHaveClass(/mobile-back-destination-active/)
    await expect(foregroundPane).toHaveCSS(
      'transform',
      /matrix\(1, 0, 0, 1, 35, 0\)/,
    )
  })
  await expect(page).toHaveURL('/')
  await expect(page.getByRole('navigation', { name: 'Rooms' })).toBeVisible()
})

test('desktop preserves selection and supports timestamp and message double-clicks', async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name === 'webkit-iphone',
    'desktop mouse behavior is covered by the desktop projects',
  )
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: {
        writeText: (value: string) => {
          const testWindow = window as typeof window & {
            __copiedText?: string
          }
          testWindow.__copiedText = value
          return Promise.resolve()
        },
      },
    })
  })
  await signIn(page)
  await page.goto(ROOM_URL)
  await expectLive(page)

  const body = `desktop gesture target ${Date.now()}`
  const row = await sendMessage(page, body)
  const message = row.locator('.event-body')
  expect(
    await message.evaluate((element) => getComputedStyle(element).userSelect),
  ).not.toBe('none')
  expect(
    await message.evaluate((element) => {
      const contextMenu = new MouseEvent('contextmenu', {
        bubbles: true,
        cancelable: true,
      })
      element.dispatchEvent(contextMenu)
      return contextMenu.defaultPrevented
    }),
  ).toBe(false)

  const timestamp = row.locator('.event-time-copy')
  await timestamp.dblclick()
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (window as typeof window & { __copiedText?: string }).__copiedText,
      ),
    )
    .toBe(body)
  await expect(row.getByRole('status')).toHaveText('Text copied')

  await message.dblclick({ position: { x: 20, y: 10 } })
  expect(
    await page.evaluate(() => window.getSelection()?.toString().length ?? 0),
  ).toBe(0)
  await expect(row.locator('.message-reaction-burst')).toHaveText('👍')
  await expect(row.locator('.reaction-chip')).toContainText('👍')
  await expect(row).not.toHaveClass(/actions-open/)

  await page.goto('/settings')
  await expect(page.getByLabel('Double tap')).toHaveValue('react')
  await page.getByLabel('Double tap').selectOption('')
  await expect(page.getByText('Gestures saved')).toBeVisible()
  await page.goto(ROOM_URL)
  await expectLive(page)

  const nativeRow = page.locator('.event-row').filter({ hasText: body }).last()
  const nativeMessage = nativeRow.locator('.event-body')
  await page.evaluate(() => window.getSelection()?.removeAllRanges())
  await nativeMessage.dblclick({ position: { x: 20, y: 10 } })
  expect(
    await page.evaluate(() => window.getSelection()?.toString().length ?? 0),
  ).toBeGreaterThan(0)
})
