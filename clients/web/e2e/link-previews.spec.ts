import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

const url =
  'http://www.nbc.com/saturday-night-live/video/centaur-job-interview/n11485'
const title = 'Watch Centaur Job Interview from Saturday Night Live on NBC.com'

test('bundled previews render in room and thread timelines without external fetches', async ({
  page,
  context,
}) => {
  const externalRequests: string[] = []
  await context.route(
    /^https?:\/\/(?:www\.nbc\.com|tracking\.test)\//,
    (route) => {
      externalRequests.push(route.request().url())
      return route.fulfill({
        contentType: 'text/html',
        body: '<title>Destination</title>',
      })
    },
  )
  await page.route(
    /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
    (route) => {
      const thread = route.request().url().includes('/threads/')
      const entries = Array.from({ length: 4 }, (_, i) => ({
        [thread ? 'matrix:matched_url' : 'matched_url']: `${url}?preview=${i}`,
        'og:title': i === 0 ? title : `Preview ${i}`,
        'og:site_name': 'NBC',
        'og:description':
          i === 0
            ? 'A doctor interviews an applicant who is a centaur.'
            : 'x'.repeat(1000),
        'og:image': 'https://tracking.test/pixel',
        'og:url': 'https://tracking.test/wrong-destination',
      }))
      const body = entries.map((_, i) => `${url}?preview=${i}`).join(' ')
      const event = {
        account_id: ACCOUNT_ID,
        room_id: ROOM_ID,
        event_id: thread ? '$preview-reply' : '$preview-root',
        sender: '@alice:hs',
        origin_ts: Date.now(),
        arrival_order: 1,
        type: 'm.room.message',
        state_key: null,
        body,
        content: {
          msgtype: 'm.text',
          body,
          [thread ? 'm.url_previews' : 'com.beeper.linkpreviews']: entries,
        },
        relates_to: thread
          ? { rel_type: 'm.thread', event_id: '$preview-root' }
          : null,
        redacted: false,
        edited: false,
        edit_count: 0,
        reactions: null,
      }
      return route.fulfill({
        json: { data: { events: [event], next_cursor: null } },
      })
    },
  )
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(ROOM_URL)
  const room = page.locator('.room-stream .link-previews').first()
  const first = room.locator('.link-preview-card').first()
  await expect(first).toContainText(title)
  await expect(first).toContainText('www.nbc.com')
  await expect(room.getByText('Preview 2', { exact: true })).toBeHidden()
  await room.locator('summary').focus()
  await page.keyboard.press('Enter')
  await expect(room.getByText('Preview 2', { exact: true })).toBeVisible()
  await room.locator('summary').click()
  await expect(room.getByText('Preview 2', { exact: true })).toBeHidden()
  expect(externalRequests).toEqual([])

  const popupPromise = page.waitForEvent('popup')
  await first.click()
  const popup = await popupPromise
  await popup.waitForLoadState()
  expect(popup.url()).toBe(`${url}?preview=0`)
  await popup.close()

  await page.goto(`${ROOM_URL}?thread=${encodeURIComponent('$preview-root')}`)
  const thread = page.locator(
    '.thread-panel [data-event-id="$preview-reply"] .link-previews',
  )
  await expect(thread.locator('.link-preview-card').first()).toContainText(
    title,
  )
  // The same cards must fit the narrow thread pane, including long words.
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(thread).toBeVisible()
  const bounds = await thread
    .locator('.link-preview-card')
    .first()
    .boundingBox()
  expect(bounds).not.toBeNull()
  expect(bounds!.x).toBeGreaterThanOrEqual(0)
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(390)
  expect(externalRequests).toEqual([`${url}?preview=0`])
})
