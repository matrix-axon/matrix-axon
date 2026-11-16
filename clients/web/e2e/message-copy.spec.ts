import { expect, test } from '@playwright/test'
import {
  ACCOUNT_ID,
  expectRoomReady,
  ROOM_ID,
  ROOM_URL,
  signIn,
} from './helpers'

const BODY = 'Copy **plain text**\nSecond line with Unicode: café 👋'

const layouts = [
  { width: 320, own: true, inspect: false },
  { width: 353, own: true, inspect: false },
  { width: 375, own: true, inspect: false },
  { width: 384, own: true, inspect: false },
  { width: 375, own: true, inspect: true },
  { width: 375, own: false, inspect: true },
  { width: 390, own: true, inspect: false },
  { width: 1400, own: true, inspect: false },
]

for (const { width, own, inspect } of layouts) {
  test(`message Copy action copies text and fits at ${width}px (${own ? 'own' : 'other'}${inspect ? ', Inspect' : ''})`, async ({
    page,
    hasTouch,
  }) => {
    await page.setViewportSize({ width, height: 844 })
    await page.addInitScript(() => {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: {
          writeText: (value: string) => {
            ;(window as typeof window & { copiedText?: string }).copiedText =
              value
            return Promise.resolve()
          },
        },
      })
    })
    await signIn(page)
    // Include both large and small bars with Inspect.
    if (inspect) {
      await page.addInitScript(() =>
        localStorage.setItem(
          'axon.settings',
          JSON.stringify({ version: 1, developerMode: true }),
        ),
      )
    }
    await page.route('**/timeline*', (route) =>
      route.fulfill({
        json: {
          data: {
            events: [
              {
                account_id: ACCOUNT_ID,
                event_id: '$copy-target',
                room_id: ROOM_ID,
                sender: own ? '@me:hs' : '@alice:hs',
                origin_ts: Date.now(),
                type: 'm.room.message',
                body: BODY,
                content: {
                  msgtype: 'm.text',
                  body: BODY,
                  format: 'org.matrix.custom.html',
                  formatted_body: '<p>Copy <strong>plain text</strong></p>',
                },
                redacted: false,
                edited: false,
                edit_count: 0,
                state_key: null,
                reactions: null,
                relates_to: null,
              },
            ],
            next_cursor: null,
          },
        },
      }),
    )
    await page.goto(ROOM_URL)
    await expectRoomReady(page)
    const row = page.locator('[data-event-id="$copy-target"]')
    const copy = row.getByRole('button', { name: 'Copy', exact: true })
    await expect(copy).toBeHidden()
    if (width < 768) {
      if (hasTouch) await row.locator('.event-body').tap()
      else await row.locator('.event-body').click()
      await expect(row).toHaveClass(/actions-open/)
    } else {
      await row.hover()
    }
    await expect(copy).toBeVisible()
    await expect(copy).toHaveAttribute(
      'title',
      'Copy text; select timestamp to copy link',
    )
    const labels = row.locator('.event-actions .event-action-label')
    expect(await labels.allTextContents()).toEqual([
      'Reply',
      'Thread',
      'Copy',
      'React',
      ...(own ? ['Edit', 'Delete'] : []),
      ...(inspect ? ['Inspect'] : []),
    ])
    const presentation = await copy.evaluate((button) => {
      const icon = button.querySelector('.event-action-icon')!
      const label = button.querySelector('.event-action-label')!
      return {
        iconDisplay: getComputedStyle(icon).display,
        labelWidth: label.getBoundingClientRect().width,
      }
    })
    expect(presentation.iconDisplay).toBe(width < 768 ? 'block' : 'none')
    if (width < 768) expect(presentation.labelWidth).toBe(1)
    else expect(presentation.labelWidth).toBeGreaterThan(10)

    const timestamp = row.getByRole('button', { name: 'Copy link' })
    if (width > 352) await expect(timestamp).toBeVisible()
    const beforeCopy = await copy.boundingBox()
    const actionsBeforeCopy = await row.locator('.event-actions').boundingBox()
    if (hasTouch) await copy.tap()
    else await copy.click()
    await expect
      .poll(() =>
        page.evaluate(
          () => (window as typeof window & { copiedText?: string }).copiedText,
        ),
      )
      .toBe(BODY)
    await expect(row.getByRole('status')).toHaveText('Copied')
    await expect(copy).toHaveAccessibleName('Copy')
    await expect(copy).toHaveText('Copy')
    expect(await copy.boundingBox()).toEqual(beforeCopy)
    expect(await row.locator('.event-actions').boundingBox()).toEqual(
      actionsBeforeCopy,
    )
    expect(
      await row
        .getByRole('status')
        .evaluate((el) => el.closest('button') === null),
    ).toBe(true)

    // Include the widest action state: an own message with delete confirmation.
    if (own) {
      await row.getByRole('button', { name: 'Delete', exact: true }).click()
      await expect(
        row.getByRole('button', { name: 'Confirm delete' }),
      ).toBeVisible()
    }
    if (width > 352 && width <= 384) {
      if (own) await expect(timestamp).toBeHidden()
      else await expect(timestamp).toBeVisible()
    }
    const bounds = await row.evaluate((element) => {
      const head = element.querySelector('.event-head')!.getBoundingClientRect()
      const actions = element
        .querySelector('.event-actions')!
        .getBoundingClientRect()
      return { left: actions.left, right: actions.right, headRight: head.right }
    })
    expect(bounds.left).toBeGreaterThanOrEqual(0)
    expect(bounds.right).toBeLessThanOrEqual(bounds.headRight + 1)
    expect(bounds.right).toBeLessThanOrEqual(width)
  })
}
