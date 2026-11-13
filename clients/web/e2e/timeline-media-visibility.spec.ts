import {
  expect,
  test,
  type Locator,
  type Page,
  type WebSocketRoute,
} from '@playwright/test'
import {
  ACCOUNT_ID,
  ROOM_ID,
  ROOM_URL,
  reloadFromInsidePage,
  shortcutForProject,
  signIn,
} from './helpers'

const LINK = 'https://example.org/article'
const TIMELINE_MEDIA =
  '.media-image, .gallery-grid, .media-tile, .media-attachment, .link-previews'

function event(
  id: string,
  order: number,
  content: Record<string, unknown>,
  roomId = ROOM_ID,
) {
  const body = String(content.body ?? id)
  return {
    account_id: ACCOUNT_ID,
    room_id: roomId,
    event_id: id,
    sender: '@other:hs',
    origin_ts: 1_800_000_000_000 + order * 1000,
    arrival_order: order,
    type: 'm.room.message',
    state_key: null,
    body,
    content: { msgtype: 'm.text', body, ...content },
    redacted: false,
    edited: false,
    edit_count: 0,
    reactions: null,
  }
}

function fixture(thread = false) {
  const prefix = thread ? '$thread-' : '$'
  const image = (id: string, order: number) =>
    event(`${prefix}${id}`, order, {
      msgtype: 'm.image',
      body: `Caption ${id} ${LINK}`,
      filename: `${id}.png`,
      url: 'mxc://hs/e2eimg',
      info: { mimetype: 'image/png', w: 320, h: 240, size: 100 },
    })
  const events = [
    image('gallery-a', 1),
    image('gallery-b', 2),
    event(`${prefix}sticker`, 3, {
      body: 'sticker.png',
      url: 'mxc://hs/e2eimg',
      info: { w: 64, h: 64, size: 100 },
    }),
    ...[
      ['video', 'm.video', 'clip.mp4', 'video/mp4'],
      ['pdf', 'm.file', 'guide.pdf', 'application/pdf'],
      ['audio', 'm.audio', 'song.mp3', 'audio/mpeg'],
      ['file', 'm.file', 'archive.zip', 'application/zip'],
    ].map(([id, msgtype, filename, mimetype], index) =>
      event(`${prefix}${id}`, 4 + index, {
        msgtype,
        filename,
        body: `Caption ${id}`,
        url: 'mxc://hs/e2eimg',
        info: { mimetype, size: 100 },
      }),
    ),
    event(`${prefix}root`, 8, {
      body: `Article ${LINK} :wave:`,
      format: 'org.matrix.custom.html',
      formatted_body: `<a href="${LINK}">Article</a> <img src="mxc://hs/e2eimg" alt="inline picture"> <img data-mx-emoticon src="mxc://hs/e2eimg" alt=":wave:">`,
      'm.url_previews': [
        {
          'matrix:matched_url': LINK,
          'og:title': 'Article preview',
          'og:image': 'mxc://hs/e2eimg',
        },
      ],
    }),
  ]
  events[2].type = 'm.sticker'
  return events.reverse().map((value) => ({
    ...value,
    relates_to: thread ? { rel_type: 'm.thread', event_id: '$root' } : null,
  }))
}

async function setup(page: Page) {
  await signIn(page)
  await page.route(
    /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
    (route) => {
      const url = new URL(route.request().url())
      const roomId = decodeURIComponent(
        url.pathname.match(/\/rooms\/([^/]+)/)![1],
      )
      return route.fulfill({
        json: {
          data: {
            events: fixture(url.pathname.includes('/threads/')).map(
              (entry) => ({ ...entry, room_id: roomId }),
            ),
            next_cursor: null,
          },
        },
      })
    },
  )
  await page.goto(ROOM_URL)
  await expect(page.locator('.room-stream .gallery-grid')).toHaveCount(1)
}

async function command(composer: Locator, text: string, accepted = true) {
  await composer.fill(text)
  if (text.startsWith('/room ')) await composer.press('Escape')
  if ((await composer.getAttribute('enterkeyhint')) === 'enter') {
    await composer
      .locator('xpath=ancestor::form')
      .getByRole('button', { name: 'Send', exact: true })
      .click()
  } else {
    await composer.press('Enter')
  }
  if (accepted) await expect(composer).toHaveValue('')
}

function roomComposer(page: Page) {
  return page.getByRole('textbox', { name: /^Message/ })
}

test('commands retain text and downloads, survive room changes, and work in threads', async ({
  page,
}, testInfo) => {
  const sends: string[] = []
  page.on('request', (request) => {
    if (
      request.method() === 'POST' &&
      /\/send(?:-media)?$/.test(new URL(request.url()).pathname)
    )
      sends.push(request.url())
  })
  await setup(page)
  const stream = page.locator('.room-stream')
  await expect(stream.locator('.link-previews')).toHaveCount(1)
  await command(roomComposer(page), '/collapse extra', false)
  await expect(roomComposer(page)).toHaveValue('/collapse extra')
  await expect(
    page.getByText('usage: /collapse', { exact: true }),
  ).toBeVisible()
  await page.getByRole('button', { name: 'Dismiss', exact: true }).click()
  await command(roomComposer(page), '/collapse')
  await command(roomComposer(page), '/collapse')
  await expect(stream.locator(TIMELINE_MEDIA)).toHaveCount(0)
  await expect(
    stream.getByRole('button', { name: 'Download gallery-a.png', exact: true }),
  ).toHaveCount(1)
  await expect(
    stream.getByText(`Caption gallery-a ${LINK}`, { exact: false }),
  ).toHaveCount(1)
  await expect(
    stream.locator('.body-html img:not([data-mx-emoticon])'),
  ).toHaveCount(0)
  await expect(stream.locator('img[data-mx-emoticon]')).toHaveCount(1)
  await expect(stream.locator('.user-avatar')).not.toHaveCount(0)
  await expect(
    stream.getByRole('link', { name: 'Article', exact: true }),
  ).toHaveAttribute('href', LINK)

  await stream.locator('.collapsed-gallery').scrollIntoViewIfNeeded()
  await expect
    .poll(() =>
      page.evaluate(
        () => document.documentElement.scrollWidth - window.innerWidth,
      ),
    )
    .toBeLessThan(2)
  await page.screenshot({ path: testInfo.outputPath('collapsed.png') })
  await command(roomComposer(page), '/room !long:hs')
  await expect(page).toHaveURL(/rooms\/!long(?::|%3A)hs$/)
  await expect(stream.locator('.media-collapsed')).not.toHaveCount(0)
  await expect(stream.locator(TIMELINE_MEDIA)).toHaveCount(0)
  await command(roomComposer(page), `/room ${ROOM_ID}`)
  await command(roomComposer(page), '/thread')
  const thread = page.locator('.thread-panel')
  await expect(thread).toBeVisible()
  await expect(thread.locator('.media-collapsed')).not.toHaveCount(0)
  await expect(thread.locator(TIMELINE_MEDIA)).toHaveCount(0)
  await command(thread.locator('textarea'), '/expand')
  await expect(thread.locator('.gallery-grid')).toHaveCount(1)
  await expect(stream.locator('.gallery-grid')).toHaveCount(1)
  await expect(
    thread.locator('[data-event-id="$thread-root"] .link-previews'),
  ).toHaveCount(1)
  await command(thread.locator('textarea'), '/expand')
  await command(thread.locator('textarea'), '/collapse')
  await expect(stream.locator(TIMELINE_MEDIA)).toHaveCount(0)
  await reloadFromInsidePage(page)
  await expect(page.locator('.room-stream .gallery-grid')).toHaveCount(1)
  expect(sends).toEqual([])
})

test('shortcuts work while typing and incoming images stay collapsed', async ({
  page,
}, testInfo) => {
  let socket: WebSocketRoute | undefined
  await page.routeWebSocket('**/v1/ws', (ws) => {
    socket = ws
    ws.connectToServer()
  })
  await setup(page)
  const composer = roomComposer(page)
  await composer.fill('Keep this draft')
  const collapse = shortcutForProject(
    testInfo.project.name,
    'Control+Alt+c',
    'Meta+Alt+c',
  )
  const expand = shortcutForProject(
    testInfo.project.name,
    'Control+Alt+e',
    'Meta+Alt+e',
  )
  await composer.press(collapse)
  await expect(
    page.locator('.room-stream').locator(TIMELINE_MEDIA),
  ).toHaveCount(0)
  await expect(composer).toHaveValue('Keep this draft')
  socket!.send(
    JSON.stringify({
      type: 'timeline.event',
      account_id: ACCOUNT_ID,
      payload: event('$incoming', 20, {
        msgtype: 'm.image',
        body: 'incoming.png',
        url: 'mxc://hs/e2eimg',
        info: { w: 320, h: 240 },
      }),
    }),
  )
  await expect(
    page.getByRole('button', { name: 'Download incoming.png', exact: true }),
  ).toHaveCount(1)
  await expect(
    page.locator('.room-stream').locator(TIMELINE_MEDIA),
  ).toHaveCount(0)
  await composer.press(expand)
  await expect(page.locator('.room-stream .gallery-grid')).toHaveCount(1)
  await expect(
    page.locator('[data-event-id="$incoming"] .media-image'),
  ).toHaveCount(1)
  await expect(composer).toHaveValue('Keep this draft')
})

test('collapsing and expanding media above a scrolled-back row preserves its position', async ({
  page,
}, testInfo) => {
  await signIn(page)
  const events = [
    ...fixture(),
    ...Array.from({ length: 35 }, (_, i) =>
      event(`$after-${i}`, 30 + i, { body: `Text after media ${i}` }),
    ),
  ]
  await page.route(/\/rooms\/[^/]+\/timeline(\?|$)/, (route) =>
    route.fulfill({
      json: {
        data: {
          events: events.sort((a, b) => b.origin_ts - a.origin_ts),
          next_cursor: null,
        },
      },
    }),
  )
  await page.goto(ROOM_URL)
  const anchor = page.locator('.room-stream [data-event-id="$after-4"]')
  await anchor.scrollIntoViewIfNeeded()
  // Place this text at the scroller's top so it becomes the retained row.
  await anchor.evaluate((row) => {
    const scroller = row.closest('.timeline')
    if (scroller instanceof HTMLElement)
      scroller.scrollTop +=
        row.getBoundingClientRect().top - scroller.getBoundingClientRect().top
  })
  await expect
    .poll(() =>
      page.locator('.room-stream .timeline').evaluate((el) => el.scrollTop),
    )
    .toBeGreaterThan(0)
  const top = await anchor.evaluate((row) => row.getBoundingClientRect().top)
  for (const key of ['c', 'e']) {
    await page.keyboard.press(
      shortcutForProject(
        testInfo.project.name,
        `Control+Alt+${key}`,
        `Meta+Alt+${key}`,
      ),
    )
    await expect(page.locator('.room-stream .gallery-grid')).toHaveCount(
      key === 'c' ? 0 : 1,
    )
    await expect
      .poll(async () =>
        Math.abs(
          (await anchor.evaluate((row) => row.getBoundingClientRect().top)) -
            top,
        ),
      )
      .toBeLessThan(3)
  }
})

for (const target of ['$gallery-a', '$gallery-b']) {
  test(`collapsed gallery jump visibly highlights ${target}`, async ({
    page,
  }, testInfo) => {
    await setup(page)
    await command(roomComposer(page), '/collapse')
    const next = `${ROOM_URL}?event=${encodeURIComponent(target)}`
    await page.evaluate((url) => {
      history.pushState(null, '', url)
      dispatchEvent(new PopStateEvent('popstate'))
    }, next)
    const item = page.locator(
      `.collapsed-gallery > li[data-event-id="${target}"]`,
    )
    await expect(
      page.locator(`.room-stream [data-event-id="${target}"]`),
    ).toHaveCount(1)
    await expect(item).toHaveClass('highlighted')
    await expect
      .poll(() => item.evaluate((el) => getComputedStyle(el).outlineWidth))
      .toBe('2px')
    const centered = await item.evaluate((el) => {
      const view = el.closest('.timeline')!.getBoundingClientRect()
      const rect = el.getBoundingClientRect()
      return rect.bottom > view.top && rect.top < view.bottom
    })
    expect(centered).toBe(true)
    await page.screenshot({
      path: testInfo.outputPath('collapsed-highlight.png'),
    })
  })
}

test('collapsed media reply quotes keep caption text without covered attachment controls', async ({
  page,
}) => {
  await signIn(page)
  const target = event('$quoted', 10, {
    msgtype: 'm.video',
    filename: 'quoted.mp4',
    body: 'Quoted video caption',
    url: 'mxc://hs/e2eimg',
    info: { mimetype: 'video/mp4' },
    format: 'org.matrix.custom.html',
    formatted_body: '<b>Quoted video caption</b>',
  })
  const reply = {
    ...event('$reply', 11, { body: 'Reply body' }),
    relates_to: { 'm.in_reply_to': { event_id: '$quoted' } },
  }
  await page.route('**/timeline*', (route) =>
    route.fulfill({
      json: { data: { events: [reply, target], next_cursor: null } },
    }),
  )
  await page.goto(ROOM_URL)
  await command(roomComposer(page), '/collapse')
  const quote = page.locator('.reply-context-body')
  await expect(quote).toContainText('Quoted video caption')
  await expect(quote.getByRole('button')).toHaveCount(0)
  await expect(quote.locator('.media-collapsed')).toHaveCount(0)
})

for (const position of ['history', 'bottom'] as const) {
  test(`thread media visibility preserves ${position} scroll position`, async ({
    page,
  }, testInfo) => {
    await signIn(page)
    const replies = [
      ...fixture(true),
      ...Array.from({ length: 40 }, (_, i) => ({
        ...event(`$thread-after-${i}`, 30 + i, {
          body: `Thread text after media ${i}`,
        }),
        relates_to: { rel_type: 'm.thread', event_id: '$root' },
      })),
    ].sort((a, b) => b.origin_ts - a.origin_ts)
    await page.route(
      /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
      (route) =>
        route.fulfill({
          json: {
            data: {
              events: route.request().url().includes('/threads/')
                ? replies
                : fixture(),
              next_cursor: null,
            },
          },
        }),
    )
    await page.goto(ROOM_URL)
    await command(roomComposer(page), '/thread')
    const scroller = page.locator('.thread-timeline')
    const anchor = page.locator(
      '.thread-panel [data-event-id="$thread-after-6"]',
    )
    await expect(anchor).toBeAttached()
    // Capture the reader's position through actual scroll events before changing row heights.
    await anchor.evaluate((row, position) => {
      const el = row.closest('.thread-timeline') as HTMLElement
      el.scrollTop =
        position === 'bottom'
          ? el.scrollHeight
          : el.scrollTop +
            row.getBoundingClientRect().top -
            el.getBoundingClientRect().top -
            80
    }, position)
    await expect
      .poll(() => scroller.evaluate((el) => el.scrollTop))
      .toBeGreaterThan(0)
    const top = await anchor.evaluate((el) => el.getBoundingClientRect().top)
    const readings = []
    for (const key of ['c', 'e']) {
      await page.keyboard.press(
        shortcutForProject(
          testInfo.project.name,
          `Control+Alt+${key}`,
          `Meta+Alt+${key}`,
        ),
      )
      await expect(page.locator('.thread-panel .gallery-grid')).toHaveCount(
        key === 'c' ? 0 : 1,
      )
      // Two frames let ResizeObserver and the sticky thread header settle.
      await page.evaluate(
        () =>
          new Promise<void>((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
          ),
      )
      const reading = await scroller.evaluate((el) => ({
        scrollTop: el.scrollTop,
        height: el.scrollHeight,
        viewport: el.clientHeight,
      }))
      const shift =
        (await anchor.evaluate((el) => el.getBoundingClientRect().top)) - top
      readings.push({ key, shift, ...reading })
      console.info(
        JSON.stringify({
          engine: testInfo.project.name,
          position,
          readings,
          capability: await page.evaluate(() => ({
            ua: navigator.userAgent,
            overflowAnchor: CSS.supports('overflow-anchor', 'auto'),
          })),
        }),
      )
      if (position === 'history') expect(Math.abs(shift)).toBeLessThan(3)
      else
        expect(
          Math.abs(reading.height - reading.viewport - reading.scrollTop),
        ).toBeLessThan(3)
    }
  })
}
