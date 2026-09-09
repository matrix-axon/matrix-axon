import { expect, test, type Page } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

const message = (id: string, ts: number, body: string, reactions = null) => ({
  account_id: ACCOUNT_ID,
  event_id: id,
  room_id: ROOM_ID,
  sender: '@alice:hs',
  origin_ts: ts,
  type: 'm.room.message',
  body,
  content: { msgtype: 'm.text', body },
  redacted: false,
  edited: false,
  edit_count: 0,
  state_key: null,
  reactions,
  relates_to: null,
})

// Timeline API order is newest-first; the client reverses it for display.
const events = [
  message('$latest', 1_023, 'newest message'),
  ...Array.from({ length: 23 }, (_, index) =>
    message(
      `$m${22 - index}`,
      1_000 + 22 - index,
      `filler message ${22 - index}`,
    ),
  ),
]

async function openReactionFixture(page: Page) {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 720 })
  await page.route('**/timeline*', (route) =>
    route.fulfill({ json: { data: { events, next_cursor: null } } }),
  )
  await page.route(/\/events\/[^/]+$/, (route) =>
    route.fulfill({
      json: {
        data: message('$latest', 1_023, 'newest message', {
          '👍': { count: 1, me: true, senders: ['@me:hs'] },
        }),
      },
    }),
  )
  await page.route(/\/events\/[^/]+\/reactions$/, (route) =>
    route.fulfill({ json: { data: { event_id: '$rx' } } }),
  )
  await page.goto(ROOM_URL)
  await expect(page.getByText('newest message')).toBeVisible()
}

async function pickerAnchorOffset(
  page: Page,
  anchor: { top: number; right: number },
) {
  return page.evaluate(({ top, right }) => {
    const dialog = document.querySelector<HTMLElement>('.reaction-full-picker')!
    const box = dialog.getBoundingClientRect()
    return {
      horizontal: Math.abs(box.left - right),
      vertical: Math.abs(box.bottom - top),
    }
  }, anchor)
}

test('reacting to the newest message keeps the reaction chip inside the timeline viewport', async ({
  page,
}) => {
  await openReactionFixture(page)

  const composer = page.getByRole('textbox', { name: 'Message E2E Room' })
  await composer.fill('/react 👍')
  await page.getByRole('button', { name: 'Send' }).click()

  await expect(page.getByText('👍 1')).toBeVisible()
  await expect
    .poll(() =>
      page.evaluate(() => {
        const timeline = document.querySelector('.timeline')!
        const chip = document.querySelector('.reaction-chip')!
        const timelineBox = timeline.getBoundingClientRect()
        const chipBox = chip.getBoundingClientRect()
        return {
          bottomOverflow: chipBox.bottom - timelineBox.bottom,
          scrollGap:
            timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight,
        }
      }),
    )
    .toMatchObject({
      bottomOverflow: expect.any(Number),
      scrollGap: 0,
    })
  const geometry = await page.evaluate(() => {
    const timeline = document.querySelector('.timeline')!
    const chip = document.querySelector('.reaction-chip')!
    return (
      chip.getBoundingClientRect().bottom -
      timeline.getBoundingClientRect().bottom
    )
  })
  expect(geometry).toBeLessThanOrEqual(0.5)
})

test('the full reaction emoji picker anchors to the more button when space allows', async ({
  page,
}) => {
  await openReactionFixture(page)
  await page.setViewportSize({ width: 1400, height: 900 })

  const row = page.locator('.event-row', { hasText: 'newest message' }).last()
  await row.locator('.event-body').click()
  await row.getByRole('button', { name: 'React' }).click()
  const moreButton = page.getByRole('button', { name: 'More reactions' })
  const anchorBox = await moreButton.evaluate((button) => {
    const box = button.getBoundingClientRect()
    return {
      top: box.top,
      right: box.right,
    }
  })
  await moreButton.click()

  const dialog = page.getByRole('dialog', { name: 'Emoji picker' })
  await expect(dialog).toBeVisible()
  await expect(dialog.locator('emoji-picker')).toBeVisible()

  // The dialog first renders at its CSS fallback, then the layout effect moves
  // it to its anchor. The picker element being visible does not prove that
  // positioning state has committed yet.
  await expect
    .poll(async () => {
      const offset = await pickerAnchorOffset(page, anchorBox)
      return Math.max(offset.horizontal, offset.vertical)
    })
    .toBeLessThanOrEqual(1)

  const geometry = await page.evaluate(() => {
    const dialog = document.querySelector<HTMLElement>('.reaction-full-picker')!
    const host = document.querySelector<HTMLElement>(
      '.reaction-full-picker-host',
    )!
    const picker = document.querySelector<HTMLElement>(
      '.reaction-full-picker-host emoji-picker',
    )!
    const dialogBox = dialog.getBoundingClientRect()
    const hostBox = host.getBoundingClientRect()
    const pickerBox = picker.getBoundingClientRect()
    const composer = document.querySelector<HTMLElement>('.composer')
    return {
      dialogHeight: dialogBox.height,
      dialogWidth: dialogBox.width,
      dialogTop: dialogBox.top,
      dialogBottom: dialogBox.bottom,
      hostHeight: hostBox.height,
      pickerHeight: pickerBox.height,
      pickerWidth: pickerBox.width,
      dialogZIndex: Number(getComputedStyle(dialog).zIndex),
      composerZIndex:
        composer === null ? 0 : Number(getComputedStyle(composer).zIndex),
      parentTag: dialog.parentElement?.tagName ?? '',
      rowContainsDialog:
        dialog.closest('.event-row') !== null ||
        dialog.closest('.reaction-picker-shell') !== null,
      viewportHeight: window.innerHeight,
      viewportWidth: window.innerWidth,
    }
  })

  expect(geometry.dialogHeight).toBeGreaterThanOrEqual(380)
  expect(geometry.dialogWidth).toBeGreaterThanOrEqual(400)
  expect(geometry.hostHeight).toBeGreaterThanOrEqual(300)
  expect(geometry.pickerHeight).toBeGreaterThanOrEqual(300)
  expect(geometry.pickerWidth).toBeGreaterThanOrEqual(400)
  expect(geometry.dialogZIndex).toBeGreaterThan(geometry.composerZIndex)
  expect(geometry.parentTag).toBe('BODY')
  expect(geometry.rowContainsDialog).toBe(false)
  expect(geometry.dialogTop).toBeGreaterThanOrEqual(0)
  expect(geometry.dialogBottom).toBeLessThanOrEqual(geometry.viewportHeight)
  expect(geometry.dialogHeight).toBeLessThanOrEqual(geometry.viewportHeight)
  expect(geometry.dialogWidth).toBeLessThanOrEqual(geometry.viewportWidth)
})

test('the full reaction emoji picker fits a short browser viewport', async ({
  page,
}) => {
  await openReactionFixture(page)
  await page.setViewportSize({ width: 1230, height: 367 })

  const row = page.locator('.event-row', { hasText: 'newest message' }).last()
  await row.locator('.event-body').click()
  await row.getByRole('button', { name: 'React' }).click()
  await page.getByRole('button', { name: 'More reactions' }).click()

  const dialog = page.getByRole('dialog', { name: 'Emoji picker' })
  await expect(dialog).toBeVisible()
  await expect(dialog.locator('emoji-picker')).toBeVisible()

  const geometry = await page.evaluate(() => {
    const dialog = document.querySelector<HTMLElement>('.reaction-full-picker')!
    const picker = document.querySelector<HTMLElement>(
      '.reaction-full-picker-host emoji-picker',
    )!
    const dialogBox = dialog.getBoundingClientRect()
    const pickerBox = picker.getBoundingClientRect()
    return {
      dialogHeight: dialogBox.height,
      dialogTop: dialogBox.top,
      dialogBottom: dialogBox.bottom,
      pickerHeight: pickerBox.height,
      viewportHeight: window.innerHeight,
    }
  })

  expect(geometry.dialogHeight).toBeGreaterThanOrEqual(300)
  expect(geometry.pickerHeight).toBeGreaterThanOrEqual(240)
  expect(geometry.dialogTop).toBeGreaterThanOrEqual(0)
  expect(geometry.dialogBottom).toBeLessThanOrEqual(geometry.viewportHeight)
})
