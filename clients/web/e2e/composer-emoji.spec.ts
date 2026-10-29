import { expect, test, type Page } from '@playwright/test'
import {
  ACCOUNT_ID,
  ROOM_ID,
  ROOM_URL,
  reloadFromInsidePage,
  shortcutForProject,
  signIn,
} from './helpers'

const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQAAAAA3bvkkAAAACklEQVQI12NoAAAAggCB3UNq9AAAAABJRU5ErkJggg==',
  'base64',
)
const ROOM_MXC = 'mxc://emoji.test/room-smile'

function event(
  id = '$emoji-root',
  body = 'Emoji fixture',
  content: unknown = { msgtype: 'm.text', body },
  thread = false,
) {
  return {
    account_id: ACCOUNT_ID,
    room_id: ROOM_ID,
    event_id: id,
    sender: '@alice:hs',
    origin_ts: 1000,
    arrival_order: 1,
    type: 'm.room.message',
    state_key: null,
    body,
    content,
    relates_to: thread
      ? { rel_type: 'm.thread', event_id: '$emoji-root' }
      : null,
    redacted: false,
    edited: false,
    edit_count: 0,
    reactions: null,
  }
}

async function fixtures(page: Page) {
  const sent: Record<string, unknown>[] = []
  let stored: unknown = null
  let available = true
  let packReads = 0
  const confirmed = new Map<string, ReturnType<typeof event>>()
  await page.route(
    /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
    (route) =>
      route.fulfill({
        json: {
          data: {
            events: [
              route.request().url().includes('/threads/')
                ? event('$emoji-reply', 'Emoji reply', undefined, true)
                : event(),
            ],
            next_cursor: null,
          },
        },
      }),
  )
  await page.route(/\/events\/[^/]+$/, (route) => {
    const id = decodeURIComponent(
      new URL(route.request().url()).pathname.split('/').at(-1)!,
    )
    return route.fulfill({ json: { data: confirmed.get(id) ?? event(id) } })
  })
  await page.route('**/emoji-packs*', (route) => {
    packReads++
    if (!available)
      return route.fulfill({
        status: 404,
        json: { error: { code: 'not_found', message: 'Unavailable' } },
      })
    return route.fulfill({
      json: {
        data: {
          partial: false,
          packs: [
            {
              room_id: null,
              state_key: null,
              event_type: 'im.ponies.user_emotes',
              display_name: 'Personal',
              images: [
                {
                  shortcode: 'smile',
                  url: 'mxc://emoji.test/personal-smile',
                  emoticon: true,
                  sticker: true,
                },
              ],
            },
            {
              room_id: ROOM_ID,
              state_key: 'Testing',
              event_type: 'm.room.image_pack',
              display_name: 'Room emoji',
              images: [
                {
                  shortcode: 'smile',
                  url: ROOM_MXC,
                  emoticon: true,
                  sticker: true,
                },
              ],
            },
          ],
        },
      },
    })
  })
  await page.route(/\/v1\/media\//, (route) =>
    route.fulfill({ contentType: 'image/png', body: PNG }),
  )
  await page.route(/\/rooms\/[^/]+\/send$/, (route) => {
    const body = route.request().postDataJSON() as Record<string, unknown>
    sent.push(body)
    const id = `$emoji-sent-${sent.length}`
    confirmed.set(
      id,
      event(
        id,
        String(body.body),
        { msgtype: 'm.text', ...body },
        Boolean(body.thread_root),
      ),
    )
    return route.fulfill({ json: { data: { event_id: id } } })
  })
  await page.route(/\/devices\/[^/]+\/state\/drafts(\?|$)/, (route) => {
    if (route.request().method() === 'PUT') {
      const body = route.request().postDataJSON() as {
        entries: Record<string, unknown>
      }
      if (Object.hasOwn(body.entries, ROOM_ID)) stored = body.entries[ROOM_ID]
      return route.fulfill({
        json: { data: { updated_at: new Date().toISOString() } },
      })
    }
    return route.fulfill({
      json: {
        data: {
          namespace: 'drafts',
          entries:
            stored === null
              ? {}
              : {
                  [ROOM_ID]: {
                    value: stored,
                    device_id: 'emoji-fixture',
                    updated_at: new Date().toISOString(),
                  },
                },
        },
      },
    })
  })
  return {
    sent,
    stored: () => stored,
    packReads: () => packReads,
    removePacks: () => {
      available = false
    },
  }
}

for (const thread of [false, true]) {
  test(`composer picker inserts Unicode and a selected custom image with native undo in the ${thread ? 'thread' : 'room'}`, async ({
    page,
  }, info) => {
    const { sent } = await fixtures(page)
    await signIn(page)
    await page.goto(
      thread
        ? `${ROOM_URL}?thread=${encodeURIComponent('$emoji-root')}`
        : ROOM_URL,
    )
    const pane = page.locator(thread ? '.thread-panel' : '.room-stream')
    const input = pane.locator('.composer textarea')
    const mod = shortcutForProject(info.project.name, 'Control', 'Meta')
    // Native editing follows the runner OS, not the emulated user agent.
    // Measured with Linux WebKit + Desktop Safari UA: Meta-Z leaves an
    // execCommand insertion intact; Control-Z restores the prior text.
    const nativeMod = process.platform === 'darwin' ? 'Meta' : 'Control'
    await input.fill('before replace after')
    await input.evaluate((el) => {
      el.focus()
      el.setSelectionRange(7, 14)
    })
    await input.press(`${mod}+;`)
    const dialog = page.getByRole('dialog', { name: 'Emoji picker' })
    const search = dialog
      .locator('emoji-picker')
      .getByRole('combobox', { name: 'Search', exact: true })
    await expect(search).toBeFocused()
    await search.fill('smile')
    await dialog.locator('emoji-picker').getByRole('option').first().click()
    await expect(input).toBeFocused()
    await expect(input).not.toHaveValue('before replace after')
    await input.press(`${nativeMod}+z`)
    await expect(input).toHaveValue('before replace after')
    await input.evaluate((el) => el.setSelectionRange(7, 14))
    await input.press(`${mod}+;`)
    await dialog.getByRole('button', { name: 'Custom', exact: true }).click()
    const custom = dialog.getByRole('searchbox', {
      name: 'Search custom emoji',
    })
    await custom.fill('smile')
    await page.screenshot({
      path: info.outputPath('composer-emoji-picker.png'),
    })
    await custom.press('ArrowDown')
    await page.keyboard.press('ArrowRight')
    await expect(
      dialog.getByRole('button', { name: 'Insert :smile:' }).nth(1),
    ).toBeFocused()
    await page.keyboard.press('Enter')
    await expect(input).toHaveValue('before :smile: after')
    await expect(input).toBeFocused()
    await input.press(`${nativeMod}+z`)
    await expect(input).toHaveValue('before replace after')
    await input.press(`${nativeMod}+Shift+z`)
    await expect(input).toHaveValue('before :smile: after')
    await pane
      .locator('.composer')
      .getByRole('button', { name: 'Send', exact: true })
      .click()
    await expect.poll(() => sent.length).toBe(1)
    expect(sent[0].body).toBe('before :smile: after')
    expect(sent[0].format).toBe('org.matrix.custom.html')
    expect(sent[0].formatted_body).toContain(`src="${ROOM_MXC}"`)
    expect(sent[0].formatted_body).toContain('data-mx-emoticon')
    if (thread) expect(sent[0].thread_root).toBe('$emoji-root')
    const img = pane.locator(
      '[data-event-id="$emoji-sent-1"] .body-html img[data-mx-emoticon]',
    )
    await expect(img).toBeVisible()
    await expect
      .poll(() => img.evaluate((el) => (el as HTMLImageElement).naturalWidth))
      .toBeGreaterThan(0)
    const height = await img.evaluate((el) => el.getBoundingClientRect().height)
    expect(height).toBeLessThan(30)
    expect(height).toBeGreaterThan(10)
    await page.screenshot({ path: info.outputPath('inline-emoji.png') })
  })
}

test('a selected image survives draft reload and pack removal without another lookup', async ({
  page,
}, info) => {
  const fixture = await fixtures(page)
  await signIn(page)
  await page.goto(ROOM_URL)
  const input = page.locator('.room-stream .composer textarea')
  const mod = shortcutForProject(info.project.name, 'Control', 'Meta')
  await input.click()
  await input.press(`${mod}+;`)
  let dialog = page.getByRole('dialog', { name: 'Emoji picker' })
  await dialog.getByRole('button', { name: 'Custom', exact: true }).click()
  await dialog.getByRole('button', { name: 'Insert :smile:' }).nth(1).click()
  await expect(input).toHaveValue(':smile:')
  await expect.poll(() => JSON.stringify(fixture.stored())).toContain(ROOM_MXC)
  fixture.removePacks()
  await reloadFromInsidePage(page)
  await expect(input).toHaveValue(':smile:')
  const reads = fixture.packReads()
  await page
    .locator('.room-stream .composer')
    .getByRole('button', { name: 'Send', exact: true })
    .click()
  await expect.poll(() => fixture.sent.length).toBe(1)
  expect(fixture.sent[0].body).toBe(':smile:')
  expect(fixture.sent[0].formatted_body).toContain(ROOM_MXC)
  expect(fixture.packReads()).toBe(reads)
  // The existing sticker surface exposes insertion on touch without a new
  // permanent composer button, and Escape preserves the draft and caret.
  await input.fill('keep this draft')
  await page
    .locator('.room-stream .composer')
    .getByRole('button', { name: 'Send sticker', exact: true })
    .click()
  await page.getByRole('button', { name: 'Insert emoji…', exact: true }).click()
  dialog = page.getByRole('dialog', { name: 'Emoji picker' })
  await expect(dialog).toBeVisible()
  const box = (await dialog.boundingBox())!
  const viewport = page.viewportSize()!
  expect(box.x).toBeGreaterThanOrEqual(0)
  expect(box.y).toBeGreaterThanOrEqual(0)
  expect(box.x + box.width).toBeLessThanOrEqual(viewport.width)
  expect(box.y + box.height).toBeLessThanOrEqual(viewport.height)
  await page.keyboard.press('Escape')
  await expect(input).toBeFocused()
  await expect(input).toHaveValue('keep this draft')
})
