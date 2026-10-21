import { expect, test, type Page } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

// A minimum-width sidebar with the spaces rail visible wraps Rooms onto a new
// row. The unfixed menu starts 44px left of that pane in WebKit at iPad widths;
// at 320px it starts 62px outside the viewport. Also cover an unwrapped row.
for (const sidebarWidth of [360, 640]) {
  for (const viewport of [
    { width: 1280, height: 960 },
    { width: 1024, height: 768 },
    { width: 768, height: 1024 },
    { width: 390, height: 844 },
    { width: 320, height: 568 },
  ]) {
    test(`Rooms menu stays inside its pane at ${viewport.width}px with ${sidebarWidth}px sidebar`, async ({
      page,
    }) => {
      await page.setViewportSize(viewport)
      await signIn(page)
      await page.addInitScript((sidebarWidth) => {
        localStorage.setItem(
          'axon.settings',
          JSON.stringify({ version: 1, sidebarWidth }),
        )
      }, sidebarWidth)
      await page.goto('/')
      await page.locator('a.room-link').first().waitFor()
      const trigger = page.locator('.room-actions-menu summary')
      await expectRoomControlsFit(page)
      await trigger.click()
      const menu = page.locator('.room-actions-popover')
      await expect(menu).toBeVisible()
      await expectPopupWithin(page, '.room-actions-popover', '.room-list-pane')
      for (const label of ['Join', 'DM', 'Create', 'Find']) {
        const link = menu.getByRole('link', { name: label, exact: true })
        await expect(link).toBeVisible()
        expect(
          await link.evaluate((el) => {
            const box = el.getBoundingClientRect()
            const hit = document.elementFromPoint(
              box.x + box.width / 2,
              box.y + box.height / 2,
            )
            return hit !== null && el.contains(hit)
          }),
        ).toBe(true)
      }
      if (viewport.width === 1280 && sidebarWidth === 640) {
        const handle = page.getByRole('button', {
          name: 'Hide rooms',
          exact: true,
        })
        await handle.press('Home')
        await expect(page.locator('.room-actions-plus')).toBeVisible()
        await expectRoomControlsFit(page)
        await expectPopupWithin(
          page,
          '.room-actions-popover',
          '.room-list-pane',
        )
        await handle.press('End')
        await expect(page.locator('.room-actions-plus')).toBeHidden()
        await expectRoomControlsFit(page)
        await expectPopupWithin(
          page,
          '.room-actions-popover',
          '.room-list-pane',
        )
      }
      await menu.getByRole('link', { name: 'Join', exact: true }).click()
      await expect(page).toHaveURL(/\/rooms\/discover#join$/)
    })
  }
}

// A wide mobile viewport has room for the text label, but keeps the phone
// presentation. The container query alone does not cover this layout.
test('a wide mobile viewport retains the compact room action', async ({
  page,
}) => {
  await page.setViewportSize({ width: 744, height: 1133 })
  await signIn(page)
  await page.goto('/')
  await page.locator('a.room-link').first().waitFor()
  await expect(page.locator('.room-actions-plus')).toBeVisible()
  await expect(page.locator('.room-actions-label')).toBeHidden()
  await expectRoomControlsFit(page)
})

for (const width of [320, 390, 1024]) {
  test(`message popups fit the timeline at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 })
    await signIn(page)
    const event = {
      account_id: ACCOUNT_ID,
      event_id: '$popup',
      room_id: ROOM_ID,
      sender: '@me:hs',
      origin_ts: 1000,
      type: 'm.room.message',
      body: 'Popup geometry fixture',
      content: { msgtype: 'm.text', body: 'Popup geometry fixture' },
      redacted: false,
      edited: false,
      edit_count: 0,
      state_key: null,
      relates_to: null,
      reactions: {
        '👍': {
          count: 1,
          me: false,
          senders: ['@a-very-long-display-name-that-must-fit:hs'],
        },
      },
    }
    await page.route('**/timeline*', (route) =>
      route.fulfill({ json: { data: { events: [event], next_cursor: null } } }),
    )
    await page.routeWebSocket('**/v1/ws', (ws) => {
      ws.send(
        JSON.stringify({
          type: 'ephemeral.passthrough',
          account_id: ACCOUNT_ID,
          payload: {
            room_id: ROOM_ID,
            event_type: 'm.receipt',
            content: {
              $popup: {
                'm.read': {
                  '@a-very-long-display-name-that-must-fit:hs': { ts: 1001 },
                },
              },
            },
          },
        }),
      )
    })
    await page.goto(ROOM_URL)
    const row = page.locator('.event-row', {
      hasText: 'Popup geometry fixture',
    })
    const chip = row.locator('.reaction-chip')
    await chip.focus()
    await expect(row.locator('.reaction-senders-tooltip')).toBeVisible()
    await expectPopupWithin(page, '.reaction-senders-tooltip')
    // Deliver Escape as soon as the popup mounts, before the next paint.
    // Visibility alone does not guarantee that a passive effect has run.
    await row.locator('.read-receipts').evaluate(
      (button) =>
        new Promise<void>((resolve) => {
          const root = button.closest('.read-receipts-wrap')!
          const observer = new MutationObserver(() => {
            if (root.querySelector('.read-receipts-popover') === null) return
            observer.disconnect()
            button.dispatchEvent(
              new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }),
            )
            resolve()
          })
          observer.observe(root, { childList: true, subtree: true })
          button.click()
        }),
    )
    await expect(row.locator('.read-receipts-popover')).toBeHidden()
    await row.locator('.read-receipts').click()
    await expect(row.locator('.read-receipts-popover')).toBeVisible()
    await expectPopupWithin(page, '.read-receipts-popover')
    await page.keyboard.press('Escape')
    await expect(row.locator('.read-receipts-popover')).toBeHidden()

    if (width < 768) {
      await row.locator('.event-body').click()
      await row.getByRole('button', { name: 'Delete', exact: true }).click()
      const confirm = row.getByRole('button', {
        name: 'Confirm delete',
        exact: true,
      })
      // Firefox's desktop profile has no TouchEvent constructor. The handler
      // needs only this bubbling event to start its long-press timer.
      await confirm.evaluate((el) =>
        el.dispatchEvent(new Event('touchstart', { bubbles: true })),
      )
      await expect(row.locator('.event-action-tooltip')).toBeVisible()
      await confirm.evaluate((el) =>
        el.dispatchEvent(new Event('touchend', { bubbles: true })),
      )
      await expectPopupWithin(page, '.event-action-tooltip')
      for (const button of await row.locator('.event-actions button').all()) {
        const box = await button.boundingBox()
        expect(box).not.toBeNull()
        expect(box!.x).toBeGreaterThanOrEqual(0)
        expect(box!.x + box!.width).toBeLessThanOrEqual(width)
      }
    }
  })
}

async function expectRoomControlsFit(page: Page) {
  const layout = await page.locator('.room-control-row').evaluate((row) => {
    const box = row.getBoundingClientRect()
    const controls = [...row.children].map((child) =>
      child.getBoundingClientRect(),
    )
    const icon = row.querySelector('.room-actions-plus')!
    const label = row.querySelector('.room-actions-label')!
    return {
      iconShown: getComputedStyle(icon).display !== 'none',
      labelShown: getComputedStyle(label).display !== 'none',
      commonTop: Math.max(...controls.map((control) => control.top)),
      commonBottom: Math.min(...controls.map((control) => control.bottom)),
      leftInset:
        Math.min(...controls.map((control) => control.left)) - box.left,
      rightInset:
        box.right - Math.max(...controls.map((control) => control.right)),
    }
  })
  // Check the rendered outcome without duplicating the CSS breakpoints: one
  // trigger presentation is shown, and every control fits on the same row.
  expect(layout.iconShown !== layout.labelShown).toBe(true)
  expect(layout.commonBottom).toBeGreaterThan(layout.commonTop)
  expect(layout.leftInset).toBeGreaterThanOrEqual(-1)
  expect(layout.rightInset).toBeGreaterThanOrEqual(-1)
  await expect(page.locator('.room-actions-menu summary')).toHaveAccessibleName(
    'Rooms',
  )
  await expect(
    page.getByRole('combobox', { name: 'Sort', exact: true }),
  ).toBeVisible()
}

async function expectPopupWithin(
  page: Page,
  selector: string,
  paneSelector = '.timeline',
) {
  const geometry = await page.locator(selector).evaluate((el, paneSelector) => {
    const box = el.getBoundingClientRect()
    const pane = el.closest(paneSelector)!.getBoundingClientRect()
    return {
      left: box.left,
      right: box.right,
      paneLeft: pane.left,
      paneRight: pane.right,
    }
  }, paneSelector)
  expect.soft(geometry.left).toBeGreaterThanOrEqual(geometry.paneLeft)
  expect.soft(geometry.right).toBeLessThanOrEqual(geometry.paneRight)
}
