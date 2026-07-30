import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, signIn } from './helpers'

/**
 * The timeline scrolls up and down, never sideways.
 *
 * `overflow-y: auto` makes a box a scroll container on *both* axes — the other
 * axis computes `visible` to `auto` — so the pane used to accept a horizontal
 * scroll it was never meant to have. Chromium at integer widths never overflows
 * enough to reveal it; a phone's sub-pixel layout does, by a pixel or two.
 *
 * Pinning it needs a real browser: jsdom has no layout, and this is entirely a
 * question of measured box widths.
 */

const message = (id: string, ts: number, body: string, html?: string) => ({
  account_id: ACCOUNT_ID,
  event_id: id,
  room_id: ROOM_ID,
  sender: '@alice:hs',
  origin_ts: ts,
  type: 'm.room.message',
  body,
  content:
    html === undefined
      ? { msgtype: 'm.text', body }
      : {
          msgtype: 'm.text',
          body,
          format: 'org.matrix.custom.html',
          formatted_body: html,
        },
  redacted: false,
  edited: false,
  edit_count: 0,
  state_key: null,
  reactions: null,
  relates_to: null,
})

/** Enough columns that it cannot wrap down to a phone's width. */
const WIDE_TABLE =
  '<table><tr>' +
  Array.from({ length: 30 }, (_, i) => `<th>header ${i}</th>`).join('') +
  '</tr><tr>' +
  Array.from({ length: 30 }, (_, i) => `<td>cell ${i}</td>`).join('') +
  '</tr></table>'

const EVENTS = [
  message('$1', 100, 'W'.repeat(300)),
  message('$2', 200, `https://example.com/${'segment/'.repeat(30)}`),
  message(
    '$3',
    300,
    'code',
    `<pre><code>${'const x = aLongFunctionCall(a, b, c, d, e, f)'.repeat(3)}</code></pre>`,
  ),
  message('$4', 400, 'table', WIDE_TABLE),
]

async function openRoomWith(
  page: import('@playwright/test').Page,
  width: number,
) {
  await signIn(page)
  await page.setViewportSize({ width, height: 844 })
  await page.route('**/timeline*', (route) =>
    route.fulfill({ json: { data: { events: EVENTS, next_cursor: null } } }),
  )
  await page.goto(`/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`)
  await expect(page.getByRole('status')).toHaveText('Live')
}

const geometry = (page: import('@playwright/test').Page) =>
  page.evaluate(() => {
    const main = document.querySelector('main')!
    const doc = document.documentElement
    // Every horizontal scroll *container* inside the pane, not just `main`. The
    // messages live in a nested `.timeline` scroller, and asserting only on
    // `main` missed a horizontal scroll region one level down — the bug that
    // shipped. A container legitimately carrying wide content (`pre`, `table`)
    // scrolls its own overflow and is expected; anything else must not.
    // Form controls (`textarea`, `select`) carry `overflow-x: auto` as a UA
    // default and soft-wrap or clip internally — they never drift the pane, so
    // they are not what this guards. The bug class is a *layout* element turned
    // into a horizontal scroll container by our own CSS.
    const FORM_CONTROLS = new Set(['textarea', 'input', 'select'])
    const horizontalScrollers: string[] = []
    for (const el of Array.from(main.querySelectorAll<HTMLElement>('*'))) {
      if (FORM_CONTROLS.has(el.tagName.toLowerCase())) {
        continue
      }
      const ox = getComputedStyle(el).overflowX
      if (ox === 'auto' || ox === 'scroll') {
        horizontalScrollers.push(
          `${el.tagName.toLowerCase()}.${String(el.className).split(' ')[0]}`,
        )
      }
    }
    return {
      overflowX: getComputedStyle(main).overflowX,
      paneOverflow: main.scrollWidth - main.clientWidth,
      documentOverflow: doc.scrollWidth - doc.clientWidth,
      horizontalScrollers,
    }
  })

for (const width of [390, 1400]) {
  test(`the timeline pane never scrolls horizontally at ${width}px`, async ({
    page,
  }) => {
    await openRoomWith(page, width)
    const geo = await geometry(page)
    expect(geo.overflowX).toBe('hidden')
    expect(geo.paneOverflow).toBe(0)
    expect(geo.documentOverflow).toBe(0)
    // The only permitted horizontal scrollers are the self-scrolling boxes for
    // wide content (`pre`, `table`). Any other — the `.timeline` scroller, a
    // row — is a horizontal scroll region the pane should not have.
    const scaffolding = geo.horizontalScrollers.filter(
      (s) => !/pre|table/.test(s),
    )
    expect(scaffolding).toEqual([])
  })
}

/**
 * Clipping the pane must not make wide content unreachable — it has to carry
 * its own scrollbar. Without this a 30-column table simply lost its right-hand
 * columns off the edge of a phone.
 */
test('wide content scrolls inside its own box instead of the pane', async ({
  page,
}) => {
  await openRoomWith(page, 390)

  const boxes = await page.evaluate(() => {
    const main = document.querySelector('main')!
    const mainRight = main.getBoundingClientRect().right
    const inspect = (selector: string) => {
      const el = document.querySelector<HTMLElement>(selector)!
      return {
        scrollsItself: el.scrollWidth > el.clientWidth + 1,
        withinPane: el.getBoundingClientRect().right <= mainRight + 0.5,
      }
    }
    return {
      pre: inspect('.body-html pre'),
      table: inspect('.body-html table'),
    }
  })

  expect(boxes.pre).toEqual({ scrollsItself: true, withinPane: true })
  expect(boxes.table).toEqual({ scrollsItself: true, withinPane: true })
})

test('formatted reply anchors stay compact on tablet width', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1024, height: 768 })
  await page.route('**/timeline*', (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            message('$target', 100, 'Yes', '<p><strong>Yes</strong></p>'),
            {
              ...message('$reply', 200, 'reply body'),
              relates_to: { 'm.in_reply_to': { event_id: '$target' } },
            },
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.goto(`/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`)
  await expect(page.getByText('reply body')).toBeVisible()

  const metrics = await page.evaluate(() => {
    const quote = document.querySelector<HTMLElement>('.reply-context')!
    const paragraph = quote.querySelector<HTMLElement>('.body-html p')!
    const paragraphStyle = getComputedStyle(paragraph)
    return {
      quoteHeight: quote.getBoundingClientRect().height,
      paragraphDisplay: paragraphStyle.display,
      paragraphMarginTop: paragraphStyle.marginTop,
      paragraphMarginBottom: paragraphStyle.marginBottom,
    }
  })

  expect(metrics.paragraphDisplay).toBe('inline')
  expect(metrics.paragraphMarginTop).toBe('0px')
  expect(metrics.paragraphMarginBottom).toBe('0px')
  expect(metrics.quoteHeight).toBeLessThan(40)
})

test('clicking a reply context jumps to its original message', async ({
  page,
}) => {
  await signIn(page)
  await page.route('**/timeline*', (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            message('$target', 100, 'original message'),
            {
              ...message('$reply', 200, 'reply body'),
              relates_to: { 'm.in_reply_to': { event_id: '$target' } },
            },
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.goto(`/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`)
  await expect(page.getByText('reply body')).toBeVisible()

  await page
    .getByRole('link', { name: 'Jump to original message from @alice:hs' })
    .click()

  await expect(page).toHaveURL(/\?event=%24target$/)
})

test('a reply anchor jumps within its open thread', async ({ page }) => {
  await signIn(page)
  const root = message('$root', 50, 'thread root')
  const earlier = {
    ...message('$earlier', 100, 'earlier thread reply'),
    relates_to: { rel_type: 'm.thread', event_id: '$root' },
  }
  const reply = {
    ...message('$reply', 200, 'later thread reply'),
    relates_to: {
      rel_type: 'm.thread',
      event_id: '$root',
      'm.in_reply_to': { event_id: '$earlier' },
    },
  }
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: [root], next_cursor: null } } }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) => {
    const atTs = new URL(route.request().url()).searchParams.get('at_ts')
    return route.fulfill({
      json: {
        data: {
          events: atTs === null ? [reply] : [earlier],
          next_cursor: null,
        },
      },
    })
  })
  await page.route(/\/events\/[^/?]+$/, (route) => {
    const eventId = decodeURIComponent(route.request().url().split('/').at(-1)!)
    if (eventId === '$earlier') {
      return route.fulfill({ json: { data: earlier } })
    }
    return route.fallback()
  })
  await page.goto(
    `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}?thread=%24root`,
  )
  await expect(page.getByText('later thread reply')).toBeVisible()

  await page.locator('.thread-panel .reply-context-link').click()

  await expect(page).toHaveURL(/\?thread=%24root&event=%24earlier$/)
  await expect(
    page.locator('.thread-panel .event-row.highlighted'),
  ).toContainText('earlier thread reply')
})

test('a reply anchor to the thread root reveals it without reloading replies', async ({
  page,
}) => {
  await signIn(page)
  const root = message('$root', 50, 'thread root')
  const reply = {
    ...message('$reply', 200, 'reply to root'),
    relates_to: {
      rel_type: 'm.thread',
      event_id: '$root',
      'm.in_reply_to': { event_id: '$root' },
    },
  }
  let threadLoads = 0
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: [root], next_cursor: null } } }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) => {
    threadLoads += 1
    return route.fulfill({
      json: { data: { events: [reply], next_cursor: null } },
    })
  })
  await page.route(/\/events\/[^/?]+$/, (route) =>
    route.fulfill({ json: { data: root } }),
  )
  await page.goto(
    `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}?thread=%24root`,
  )
  await expect(page.getByText('reply to root')).toBeVisible()

  await page.locator('.thread-panel .reply-context-link').click()

  await expect(page).toHaveURL(/\?thread=%24root&event=%24root$/)
  expect(threadLoads).toBe(1)
})

test('a cold thread-root deep link loads the reply timeline', async ({
  page,
}) => {
  await signIn(page)
  const root = message('$root', 50, 'thread root')
  const reply = {
    ...message('$reply', 200, 'reply to root'),
    relates_to: {
      rel_type: 'm.thread',
      event_id: '$root',
      'm.in_reply_to': { event_id: '$root' },
    },
  }
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: [root], next_cursor: null } } }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: { data: { events: [reply], next_cursor: null } },
    }),
  )
  await page.route(/\/events\/[^/?]+$/, (route) =>
    route.fulfill({ json: { data: root } }),
  )

  await page.goto(
    `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}?thread=%24root&event=%24root`,
  )

  await expect(page.getByText('reply to root')).toBeVisible()
  await expect(page.getByText('Loading thread…')).toHaveCount(0)
})

test('thread reply anchors preserve modified-click link behavior', async ({
  page,
}) => {
  await signIn(page)
  const root = message('$root', 50, 'thread root')
  const earlier = {
    ...message('$earlier', 100, 'earlier thread reply'),
    relates_to: { rel_type: 'm.thread', event_id: '$root' },
  }
  const reply = {
    ...message('$reply', 200, 'later thread reply'),
    relates_to: {
      rel_type: 'm.thread',
      event_id: '$root',
      'm.in_reply_to': { event_id: '$earlier' },
    },
  }
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: [root], next_cursor: null } } }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: { data: { events: [reply], next_cursor: null } },
    }),
  )
  await page.route(/\/events\/[^/?]+$/, (route) =>
    route.fulfill({ json: { data: earlier } }),
  )
  await page.goto(
    `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}?thread=%24root`,
  )
  await expect(page.getByText('later thread reply')).toBeVisible()

  const prevented = await page
    .locator('.thread-panel .reply-context-link')
    .evaluate((link) => {
      const click = new MouseEvent('click', {
        bubbles: true,
        cancelable: true,
        ctrlKey: true,
      })
      link.dispatchEvent(click)
      return click.defaultPrevented
    })

  expect(prevented).toBe(false)
})

test('a long thread root pins without changing the reply scroll geometry', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  const root = message('$long-root', 50, 'Long root. '.repeat(80))
  const replies = Array.from({ length: 12 }, (_, index) => ({
    ...message(`$reply-${index}`, 100 + index, `reply ${index} `.repeat(12)),
    relates_to: { rel_type: 'm.thread', event_id: '$long-root' },
  }))
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: [root], next_cursor: null } } }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: replies, next_cursor: null } } }),
  )
  await page.route(/\/events\/[^/?]+$/, (route) =>
    route.fulfill({ json: { data: root } }),
  )
  await page.goto(
    `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}?thread=%24long-root`,
  )
  await expect(page.getByText('reply 11')).toBeVisible()

  const timeline = page.locator('.thread-timeline')
  const preview = page.locator('.thread-root-preview')
  const scrollHeight = await timeline.evaluate(
    (element) => element.scrollHeight,
  )
  await timeline.evaluate((element) => {
    element.scrollTop = element.scrollHeight
    element.dispatchEvent(new Event('scroll'))
  })
  await expect(preview).toHaveCount(1)
  const [timelineBox, previewBox, fullRootBox] = await Promise.all([
    timeline.boundingBox(),
    preview.boundingBox(),
    timeline.locator('.thread-root:not(.compact)').boundingBox(),
  ])
  expect(timelineBox).not.toBeNull()
  expect(previewBox).not.toBeNull()
  expect(fullRootBox).not.toBeNull()
  expect(previewBox!.y).toBeCloseTo(timelineBox!.y, 0)
  expect(fullRootBox!.y + fullRootBox!.height).toBeLessThanOrEqual(
    timelineBox!.y,
  )
  await expect
    .poll(() => timeline.evaluate((element) => element.scrollHeight))
    .toBe(scrollHeight)

  await timeline.evaluate((element) => {
    const root = element.querySelector<HTMLElement>(
      '.thread-root:not(.compact)',
    )
    element.scrollTop = Math.max(0, (root?.offsetHeight ?? 1) - 1)
    element.dispatchEvent(new Event('scroll'))
  })
  await expect(preview).toHaveCount(0)
  await expect(timeline.locator('.thread-root:not(.compact)')).toBeVisible()
})

for (const { name, width } of [
  { name: 'mobile', width: 390 },
  { name: 'desktop', width: 1400 },
]) {
  test(`a page-long ${name} thread root keeps its own scrollport`, async ({
    page,
  }) => {
    await signIn(page)
    await page.setViewportSize({ width, height: 900 })
    const root = message(
      `$${name}-long-root`,
      50,
      `${name} long root. `.repeat(600),
    )
    await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
      route.fulfill({ json: { data: { events: [root], next_cursor: null } } }),
    )
    await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
      route.fulfill({ json: { data: { events: [], next_cursor: null } } }),
    )
    await page.route(/\/events\/[^/?]+$/, (route) =>
      route.fulfill({ json: { data: root } }),
    )
    await page.goto(
      `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}?thread=%24${name}-long-root`,
    )
    await expect(page.locator('.thread-panel')).toContainText(
      `${name} long root.`,
    )

    const timeline = page.locator('.thread-timeline')
    const dimensions = await timeline.evaluate((element) => ({
      clientHeight: element.clientHeight,
      scrollHeight: element.scrollHeight,
    }))
    expect(dimensions.clientHeight).toBeGreaterThan(0)
    expect(dimensions.scrollHeight).toBeGreaterThan(dimensions.clientHeight)
    const scrollTop = await timeline.evaluate((element) => {
      element.scrollTop = element.scrollHeight
      return element.scrollTop
    })
    expect(scrollTop).toBeGreaterThan(0)
  })
}

test('reply-context links use the same no-underline style as message links', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1024, height: 768 })
  await page.route('**/timeline*', (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            message(
              '$target',
              100,
              'https://example.org/docs',
              '<p><a href="https://example.org/docs">https://example.org/docs</a></p>',
            ),
            {
              ...message('$reply', 200, 'reply body'),
              relates_to: { 'm.in_reply_to': { event_id: '$target' } },
            },
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.goto(`/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`)
  await expect(page.getByText('reply body')).toBeVisible()

  const styles = await page.evaluate(() => {
    const replyLink = document.querySelector<HTMLElement>(
      '.reply-context a[href="https://example.org/docs"]',
    )!
    return {
      textDecorationLine: getComputedStyle(replyLink).textDecorationLine,
      fontWeight: getComputedStyle(replyLink).fontWeight,
    }
  })

  expect(styles.textDecorationLine).toBe('none')
  expect(Number(styles.fontWeight)).toBeGreaterThanOrEqual(500)
})
