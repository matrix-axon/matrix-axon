import { expect, test, type Locator } from '@playwright/test'
import {
  ACCOUNT_ID,
  active,
  expectPaneCenterUncovered,
  openRoom,
  ROOM_ID,
  ROOM_URL,
  shown,
  signIn,
  width,
} from './helpers'

/**
 * The two-pane layout (ADR 0062). Everything here is decided by CSS, so it can
 * only be proven in a real browser: computed `display`, the widths panes
 * actually take, and whether the document overflows. Unit tests see none of it.
 */
test.describe.configure({ mode: 'serial' })

test('wide: sidebar beside the room-entry pane and timeline', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })

  await page.goto('/')
  await expect(page.locator('.shell-body')).toHaveClass(/mode-rooms/)
  expect(await shown(page, '.sidebar')).toBe('visible')
  await expect(page.getByRole('heading', { name: 'Add a Room' })).toBeVisible()

  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')
  await expect(page.locator('.shell-body')).toHaveClass(/mode-room/)
  expect(await shown(page, '.sidebar')).toBe('visible')
  expect(await shown(page, '.timeline')).toBe('visible')
})

test('wide: spaces use a compact rail that can be hidden independently', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')

  const roomList = page.locator('.room-list-pane')
  await expect(page.locator('.space-picker')).toBeVisible()
  const widthWithSpaces = await width(page, '.room-list-pane')

  await page.getByRole('button', { name: 'Hide spaces' }).click()
  await expect(page.locator('.space-picker')).toBeHidden()
  await expect(page.getByRole('button', { name: 'Show spaces' })).toBeVisible()
  expect(await width(page, '.room-list-pane')).toBeGreaterThan(widthWithSpaces!)

  await page.getByRole('button', { name: 'Show spaces' }).click()
  await expect(page.locator('.space-picker')).toBeVisible()
  await expect(roomList).toBeVisible()
})

test('wide: space icons stay fully inside their rail at Windows scaling', async ({
  page,
}) => {
  // 1536 CSS pixels is a 1920px-wide display at 125% Windows scaling.
  await signIn(page)
  await page.setViewportSize({ width: 1536, height: 864 })
  await page.goto('/')
  await expect(
    page.getByRole('button', { name: 'E2E Space One' }),
  ).toBeVisible()

  const geometry = await page.evaluate(() => {
    const rail = document.querySelector<HTMLElement>('.space-picker')!
    const railBox = rail.getBoundingClientRect()
    return [...rail.querySelectorAll<HTMLElement>('.space-picker-entry')].map(
      (entry) => {
        const box = entry.getBoundingClientRect()
        return {
          left: box.left - railBox.left,
          right: railBox.right - box.right,
          railOverflow: rail.scrollWidth - rail.clientWidth,
        }
      },
    )
  })

  expect(geometry.length).toBeGreaterThan(2)
  for (const entry of geometry) {
    expect(entry.left).toBeGreaterThanOrEqual(0)
    expect(entry.right).toBeGreaterThanOrEqual(0)
    expect(entry.railOverflow).toBeLessThanOrEqual(0)
  }
})

test('wide: space scrollbar appears only while the pane is active', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1536, height: 864 })
  await page.goto('/')
  const rail = page.locator('.space-picker')

  await expect(rail).toHaveCSS(
    'scrollbar-color',
    'rgba(0, 0, 0, 0) rgba(0, 0, 0, 0)',
  )
  await rail.hover()
  await expect(rail).toHaveCSS(
    'scrollbar-color',
    'rgb(106, 106, 116) rgba(0, 0, 0, 0)',
  )
})

test('wide: room-list scrollbar appears only while the pane is active', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1536, height: 864 })
  await page.goto('/')
  const list = page.locator('.room-list-pane')

  await expect(list).toHaveCSS(
    'scrollbar-color',
    'rgba(0, 0, 0, 0) rgba(0, 0, 0, 0)',
  )
  await list.hover()
  await expect(list).toHaveCSS(
    'scrollbar-color',
    'rgb(106, 106, 116) rgba(0, 0, 0, 0)',
  )
})

test('space avatars retain an accessible button without button chrome', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1536, height: 864 })
  await page.goto('/')
  const space = page.getByRole('button', { name: 'E2E Space One' })

  await expect(space).toHaveCSS('border-top-width', '0px')
  await expect(space).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
  await space.focus()
  await expect(space).toHaveCSS('outline-style', 'solid')
  await expect(page.locator('.space-picker')).toHaveCSS(
    'scrollbar-color',
    'rgba(0, 0, 0, 0) rgba(0, 0, 0, 0)',
  )
})

test('narrow: room-list topbar uses icons instead of wrapping labels', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')

  await expect(page.locator('.topbar-icon').first()).toHaveCSS(
    'display',
    'block',
  )
  await expect(page.locator('.topbar-label').first()).toHaveCSS(
    'position',
    'absolute',
  )
  const labelBox = await page.locator('.topbar-label').first().boundingBox()
  expect(labelBox?.width).toBeLessThanOrEqual(1)
  await expect(page.getByRole('link', { name: 'Settings' })).toBeVisible()
  await expect(
    page.getByRole('button', { name: 'Unread threads' }),
  ).toBeVisible()
  await expect(
    page.getByRole('button', { name: 'Search messages' }),
  ).toBeVisible()

  const height = await page.locator('.topbar').evaluate((element) => {
    return element.getBoundingClientRect().height
  })
  expect(height).toBeLessThanOrEqual(64)
})

test('narrow: the room list fills the single-pane viewport', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')

  const geometry = await page.evaluate(() => {
    const sidebar = document.querySelector<HTMLElement>('.sidebar')!
    const roomList = document.querySelector<HTMLElement>('.room-list-pane')!
    return {
      viewportWidth: window.innerWidth,
      sidebarRight: sidebar.getBoundingClientRect().right,
      roomListRight: roomList.getBoundingClientRect().right,
    }
  })

  expect(geometry.sidebarRight).toBeGreaterThanOrEqual(
    geometry.viewportWidth - 1,
  )
  expect(geometry.roomListRight).toBeGreaterThanOrEqual(
    geometry.viewportWidth - 1,
  )
})

test('the room list survives a room switch, filter and all', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')

  await page.getByLabel('Filter by name').fill('room')
  await page.locator('a.room-link').first().click()
  await expect(page).toHaveURL(/\/rooms\//)

  // The sidebar was never unmounted, so its session-only filter is still set.
  await expect(page.getByLabel('Filter by name')).toHaveValue('room')
})

test('wide: the thread is a third column that shrinks the timeline', async ({
  page,
}) => {
  await openRoom(page)
  const alone = await width(page, '.room-stream')

  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(page.locator('.thread-panel')).toBeVisible()

  const position = await page.evaluate(
    () => getComputedStyle(document.querySelector('.thread-panel')!).position,
  )
  expect(position).toBe('static') // a real pane, not an overlay
  expect(await width(page, '.room-stream')).toBeLessThan(alone!)
  expect(await shown(page, '.sidebar')).toBe('visible') // never covered
})

test('reload strips the restored thread view from a room URL', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(page.locator('.thread-panel')).toBeVisible()

  await page.reload()

  await expect(page).not.toHaveURL(/thread=/)
  await expect(page.locator('.thread-panel')).toHaveCount(0)
  await expect(page.locator('.timeline')).toBeVisible()
})

test('mid width: two panes, but the thread falls back to an overlay', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 880, height: 900 }) // 55rem
  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(page.locator('.thread-panel')).toBeVisible()

  expect(await shown(page, '.sidebar')).toBe('visible')
  const position = await page.evaluate(
    () => getComputedStyle(document.querySelector('.thread-panel')!).position,
  )
  expect(position).toBe('fixed')
})

test('narrow: one pane, chosen by the route', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 640, height: 900 }) // 40rem

  await page.goto('/')
  expect(await shown(page, '.sidebar')).toBe('visible')
  expect(await shown(page, '.shell main')).toBe('hidden')

  await page.locator('a.room-link').first().click()
  await expect(page).toHaveURL(/\/rooms\//)
  expect(await shown(page, '.sidebar')).toBe('hidden')
  expect(await shown(page, '.shell main')).toBe('visible')
  // Focus lands on the visible mobile room title, not the link that went away.
  await expect.poll(() => active(page)).toContain('Open room information')

  await page.goBack()
  expect(await shown(page, '.sidebar')).toBe('visible')
})

test('narrow: touch tap opens a room without a click event', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')

  const firstRoom = page.locator('a.room-link').first()
  await expect(firstRoom).toBeVisible()
  // A stationary down+up pair is a tap; navigation happens on pointerup so
  // that touch *scroll* gestures over the list never open a room.
  await firstRoom.dispatchEvent('pointerdown', {
    pointerType: 'touch',
    pointerId: 7,
  })
  await firstRoom.dispatchEvent('pointerup', {
    pointerType: 'touch',
    pointerId: 7,
  })

  await expect(page).toHaveURL(/\/rooms\//)
  expect(await shown(page, '.sidebar')).toBe('hidden')
  expect(await shown(page, '.shell main')).toBe('visible')
})

test('narrow: thread drawer stacks above the room composer', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(page.locator('.thread-panel')).toBeVisible()

  const stacking = await page.evaluate(() => {
    const drawer = document.querySelector<HTMLElement>('.thread-panel')!
    const roomComposer = document.querySelector<HTMLElement>(
      '.room-stream > .composer',
    )!
    const drawerBox = drawer.getBoundingClientRect()
    return {
      drawerPosition: getComputedStyle(drawer).position,
      drawerZ: Number(getComputedStyle(drawer).zIndex),
      roomComposerZ: Number(getComputedStyle(roomComposer).zIndex),
      drawerLeft: drawerBox.left,
      drawerRight: drawerBox.right,
      viewportWidth: window.innerWidth,
    }
  })

  expect(stacking.drawerPosition).toBe('fixed')
  expect(stacking.drawerZ).toBeGreaterThan(stacking.roomComposerZ)
  expect(stacking.drawerLeft).toBe(0)
  expect(stacking.drawerRight).toBe(stacking.viewportWidth)
})

test('standalone iOS thread composer stays on screen when keyboard is dismissed', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'platform', {
      configurable: true,
      get: () => 'iPhone',
    })
    Object.defineProperty(navigator, 'userAgent', {
      configurable: true,
      get: () =>
        'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15',
    })
    Object.defineProperty(navigator, 'userAgentData', {
      configurable: true,
      value: { platform: 'iOS' },
    })
    Object.defineProperty(navigator, 'standalone', {
      configurable: true,
      value: true,
    })
    const originalMatchMedia = window.matchMedia.bind(window)
    window.matchMedia = (query: string): MediaQueryList => {
      if (query === '(display-mode: standalone)') {
        return {
          media: query,
          matches: true,
          onchange: null,
          addListener: () => {},
          removeListener: () => {},
          addEventListener: () => {},
          removeEventListener: () => {},
          dispatchEvent: () => false,
        } as MediaQueryList
      }
      return originalMatchMedia(query)
    }
  })

  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(
    page.getByRole('complementary', { name: 'Thread' }),
  ).toBeVisible()

  const geometry = await page.evaluate(() => {
    const drawer = document.querySelector<HTMLElement>('.thread-panel')!
    const composer = document.querySelector<HTMLElement>(
      '.thread-panel > .composer',
    )!
    const drawerBox = drawer.getBoundingClientRect()
    const composerBox = composer.getBoundingClientRect()
    return {
      drawerBottom: drawerBox.bottom,
      composerBottom: composerBox.bottom,
      viewportBottom: window.innerHeight,
      boxSizing: getComputedStyle(drawer).boxSizing,
    }
  })

  expect(geometry.boxSizing).toBe('border-box')
  expect(geometry.drawerBottom).toBeLessThanOrEqual(geometry.viewportBottom)
  expect(geometry.composerBottom).toBeLessThanOrEqual(geometry.viewportBottom)
})

test('utility pages are full-width with no sidebar', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/accounts')

  await expect(page.locator('.shell-body')).toHaveClass(/mode-utility/)
  expect(await shown(page, '.sidebar')).toBe('hidden')
  // The old centered 56rem column, which now lives only here.
  expect(await width(page, '.shell main')).toBeLessThanOrEqual(56 * 16 + 1)
})

test('narrow: settings back navigation leaves the timeline uncovered', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(ROOM_URL)
  await expect(page.locator('.timeline')).toBeVisible()

  await page.getByRole('link', { name: 'Settings' }).click()
  await expect(page.getByRole('heading', { name: 'Settings' })).toBeVisible()
  await page.goBack()

  await expect(page).toHaveURL(ROOM_URL)
  await expect(page.locator('.timeline')).toBeVisible()
  await expectPaneCenterUncovered(page, '.timeline')
})

test('narrow: starting a DM from room information uncovers the new timeline', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route(/\/rooms\/[^/]+\/members$/, (route) =>
    route.fulfill({
      json: {
        data: [
          {
            user_id: '@me:hs',
            membership: 'join',
            display_name: 'Me',
            avatar_url: null,
          },
          {
            user_id: '@alice:hs',
            membership: 'join',
            display_name: 'Alice',
            avatar_url: null,
          },
        ],
      },
    }),
  )
  await page.route(/\/rooms\/dm$/, (route) =>
    route.fulfill({ json: { data: { room_id: '!dm:hs' } } }),
  )

  await page.goto(ROOM_URL)
  await expect(page.locator('.timeline')).toBeVisible()
  await page.getByRole('button', { name: 'Open room information' }).click()
  const panel = page.getByRole('complementary', { name: 'Room information' })
  await expect(panel).toBeVisible()

  await panel
    .getByRole('button', { name: 'Open DM with Alice (@alice:hs)' })
    .click()

  await expect(page).toHaveURL(
    `/${ACCOUNT_ID}/rooms/${encodeURIComponent('!dm:hs')}`,
  )
  await expect(panel).toHaveCount(0)
  await expect(page.locator('.timeline')).toBeVisible()
  await expectPaneCenterUncovered(page, '.timeline')
})

test('room information derives a parent space from its child relationship', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(ROOM_URL)

  await page.getByRole('button', { name: 'Open room information' }).click()

  const panel = page.getByRole('complementary', { name: 'Room information' })
  await expect(
    panel.getByRole('button', { name: 'Parent: E2E Space One' }),
  ).toBeVisible()
})

test('narrow: room information scrolls when the member list overflows', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  const members = [
    {
      user_id: '@me:hs',
      membership: 'join',
      display_name: 'Me',
      avatar_url: null,
    },
    ...Array.from({ length: 30 }, (_, index) => {
      const number = String(index + 1).padStart(2, '0')
      return {
        user_id: `@member${number}:hs`,
        membership: 'join',
        display_name: `Member ${number}`,
        avatar_url: null,
      }
    }),
  ]
  await page.route(/\/rooms\/[^/]+\/members$/, (route) =>
    route.fulfill({ json: { data: members } }),
  )

  await page.goto(ROOM_URL)
  await expect(page.locator('.timeline')).toBeVisible()
  await page
    .getByRole('banner')
    .getByRole('button', { name: 'Open room information' })
    .click()
  const panel = page.getByRole('complementary', { name: 'Room information' })
  await expect(panel).toBeVisible()

  const initial = await panel.evaluate((element) => ({
    clientHeight: element.clientHeight,
    overflowY: getComputedStyle(element).overflowY,
    scrollHeight: element.scrollHeight,
    scrollTop: element.scrollTop,
  }))
  expect(initial.overflowY).toBe('auto')
  expect(initial.scrollHeight).toBeGreaterThan(initial.clientHeight)
  expect(initial.scrollTop).toBe(0)

  await panel.hover()
  await page.mouse.wheel(0, 5000)
  await expect
    .poll(() => panel.evaluate((element) => element.scrollTop))
    .toBeGreaterThan(0)

  const lastMember = panel.getByText('Member 30', { exact: true })
  await expect(lastMember).toBeVisible()
  await expect
    .poll(() =>
      lastMember.evaluate((element) => {
        const panel = element.closest<HTMLElement>('.room-info-panel')
        if (panel === null) {
          return false
        }
        const panelBox = panel.getBoundingClientRect()
        const memberBox = element.getBoundingClientRect()
        return (
          memberBox.top >= panelBox.top && memberBox.bottom <= panelBox.bottom
        )
      }),
    )
    .toBe(true)
})

test('collapsing the sidebar survives a reload', async ({ page }) => {
  await openRoom(page)

  await page.getByRole('button', { name: 'Hide rooms' }).click()
  expect(await shown(page, '.sidebar')).toBe('hidden')

  await page.reload()
  await expect(page.getByRole('button', { name: 'Show rooms' })).toBeVisible()
  expect(await shown(page, '.sidebar')).toBe('hidden')

  await page.getByRole('button', { name: 'Show rooms' }).click()
  expect(await shown(page, '.sidebar')).toBe('visible')
})

test('the collapsed rooms tab stays fully on screen', async ({ page }) => {
  await openRoom(page)
  await page.getByRole('button', { name: 'Hide rooms' }).click()

  // Collapsed, this tab is the only pointer route back to the room list, and
  // `.shell { overflow: hidden }` clips anything past the viewport edge.
  const tab = page.getByRole('button', { name: 'Show rooms' })
  const box = await tab.boundingBox()
  expect(box).not.toBeNull()
  expect(box!.x).toBeGreaterThanOrEqual(0)
  const pill = await tab.locator('span').boundingBox()
  expect(pill).not.toBeNull()
  expect(pill!.x).toBeGreaterThanOrEqual(0)
})

test('the collapsed spaces tab stays fully on screen', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')
  await page.getByRole('button', { name: 'Hide spaces' }).click()

  // Collapsed, the picker is `display: none`, so this tab becomes the sidebar's
  // first child — the same negative-margin trap as the rooms tab above, and the
  // only pointer route back to the picker.
  const tab = page.getByRole('button', { name: 'Show spaces' })
  const box = await tab.boundingBox()
  expect(box).not.toBeNull()
  expect(box!.x).toBeGreaterThanOrEqual(0)
  await tab.click()
  await expect(page.locator('.space-picker')).toBeVisible()
})

test('narrow: a collapsed spaces pane is never stranded', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')
  // Collapse on desktop, where the toggle exists, then narrow the window: the
  // toggle is gone at this width and `mod+alt+s` is inert, so honoring the
  // persisted collapse would leave no way back.
  await page.getByRole('button', { name: 'Hide spaces' }).click()
  await expect(page.locator('.space-picker')).toBeHidden()

  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.locator('.space-picker')).toBeVisible()
})

test('narrow: spaces can be reordered without a keyboard', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')

  // A phone has no Ctrl-Alt-R, and touch drag-and-drop is uneven across
  // engines, so the toggle has to be reachable at this width.
  const toggle = page.getByRole('button', { name: 'Reorder spaces' })
  await expect(toggle).toBeVisible()
  const box = await toggle.boundingBox()
  expect(box).not.toBeNull()
  expect(box!.x).toBeGreaterThanOrEqual(0)

  const widthBefore = await page.evaluate(
    () => document.querySelector<HTMLElement>('.space-picker')!.scrollWidth,
  )
  await toggle.click()
  // The ◀ ▶ pair sits under its avatar, so turning the mode on must not make
  // the strip any wider — only taller.
  expect(
    await page.evaluate(
      () => document.querySelector<HTMLElement>('.space-picker')!.scrollWidth,
    ),
  ).toBeLessThanOrEqual(widthBefore)
  // Horizontal strip: the axis is left/right here, not up/down.
  const forward = page.getByRole('button', {
    name: 'Move E2E Space One right',
  })
  await expect(forward).toBeVisible()
  await forward.click()

  const order = await page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>('[data-space-key]')].map(
      (button) => button.getAttribute('aria-label'),
    ),
  )
  expect(order).toEqual(['E2E Space Two', 'E2E Space One'])
})

test('the rooms edge tab drags to resize without collapsing', async ({
  page,
}) => {
  await openRoom(page)
  const sidebar = page.locator('.sidebar')
  const before = await width(page, '.sidebar')
  const tab = page.getByRole('button', { name: 'Hide rooms' })
  const box = await tab.boundingBox()
  expect(box).not.toBeNull()

  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2)
  await page.mouse.down()
  await page.mouse.move(box!.x + box!.width / 2 + 80, box!.y + box!.height / 2)
  await page.mouse.up()

  await expect.poll(() => width(page, '.sidebar')).toBeGreaterThan(before!)
  await expect(sidebar).toBeVisible()
  await expect(tab).toHaveAttribute('aria-expanded', 'true')
})

test('the rooms edge tab sits close to the favorite controls', async ({
  page,
}) => {
  await openRoom(page)
  const favorite = page.locator('.room-row').first().getByRole('button', {
    name: '☆',
  })
  const tab = page.getByRole('button', { name: 'Hide rooms' }).locator('span')

  const [favoriteBox, tabBox] = await Promise.all([
    favorite.boundingBox(),
    tab.boundingBox(),
  ])
  expect(tabBox).not.toBeNull()
  expect(favoriteBox).not.toBeNull()
  expect(tabBox!.x - (favoriteBox!.x + favoriteBox!.width)).toBeLessThanOrEqual(
    16,
  )
})

test('topbar stays pinned when the composer is focused', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')

  const composer = page.getByRole('textbox', { name: /^Message/ })
  const emptyComposer = await composer.evaluate((el) => {
    const textarea = el as HTMLTextAreaElement
    return {
      height: textarea.getBoundingClientRect().height,
      lineHeight: Number.parseFloat(getComputedStyle(textarea).lineHeight),
      paddingTop: Number.parseFloat(getComputedStyle(textarea).paddingTop),
      paddingBottom: Number.parseFloat(
        getComputedStyle(textarea).paddingBottom,
      ),
      placeholder: textarea.placeholder,
      value: textarea.value,
    }
  })
  expect(emptyComposer.value).toBe('')
  expect(emptyComposer.placeholder).toBe('Message E2E Room')
  expect(emptyComposer.height).toBeLessThanOrEqual(
    emptyComposer.lineHeight +
      emptyComposer.paddingTop +
      emptyComposer.paddingBottom +
      4,
  )

  await composer.fill('typing should not move the app shell')
  await expect(
    page
      .getByRole('banner')
      .getByRole('button', { name: 'Open room information' }),
  ).toBeVisible()
  await expect(page.getByRole('button', { name: 'Jump' })).toHaveCount(0)
  await expect(page.locator('.room-head')).toBeHidden()
  await expect(page.locator('.topbar-room-title')).toContainText('E2E Room')

  const geometry = await page.evaluate(() => {
    const topbar = document.querySelector<HTMLElement>('.topbar')!
    const shell = document.querySelector<HTMLElement>('.shell')!
    const composer = document.querySelector<HTMLElement>('.composer')!
    const timeline = document.querySelector<HTMLElement>('.timeline')!
    return {
      scrollY: window.scrollY,
      top: topbar.getBoundingClientRect().top,
      topGap:
        timeline.getBoundingClientRect().top -
        topbar.getBoundingClientRect().bottom,
      topbarPosition: getComputedStyle(topbar).position,
      shellPosition: getComputedStyle(shell).position,
      shellTop: shell.getBoundingClientRect().top,
      bottomGap:
        shell.getBoundingClientRect().bottom -
        composer.getBoundingClientRect().bottom,
      bodyOverflow: getComputedStyle(document.body).overflow,
    }
  })

  expect(geometry.scrollY).toBe(0)
  expect(geometry.top).toBe(0)
  expect(geometry.topGap).toBeLessThanOrEqual(12)
  expect(geometry.topbarPosition).toBe('sticky')
  expect(geometry.shellPosition).toBe('fixed')
  expect(geometry.shellTop).toBe(0)
  expect(geometry.bottomGap).toBeLessThanOrEqual(8)
  expect(geometry.bodyOverflow).toBe('hidden')
})

test('standalone iOS focus inset leaves only a small composer gap', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  const newest = 'standalone newest message'
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            ...Array.from({ length: 7 }, (_, index) =>
              sparseEvent(
                `$standalone-${index}`,
                `standalone message ${index}`,
              ),
            ),
            sparseEvent('$standalone-newest', newest),
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'platform', {
      configurable: true,
      get: () => 'iPhone',
    })
    Object.defineProperty(navigator, 'userAgent', {
      configurable: true,
      get: () =>
        'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15',
    })
    Object.defineProperty(navigator, 'userAgentData', {
      configurable: true,
      value: { platform: 'iOS' },
    })
    Object.defineProperty(navigator, 'standalone', {
      configurable: true,
      value: true,
    })
    const originalMatchMedia = window.matchMedia.bind(window)
    window.matchMedia = (query: string): MediaQueryList => {
      if (query === '(display-mode: standalone)') {
        return {
          media: query,
          matches: true,
          onchange: null,
          addListener: () => {},
          removeListener: () => {},
          addEventListener: () => {},
          removeEventListener: () => {},
          dispatchEvent: () => false,
        } as MediaQueryList
      }
      return originalMatchMedia(query)
    }
  })
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')
  await expect(page.locator('html')).toHaveCSS(
    '--app-standalone-composer-bottom-padding',
    '4px',
  )

  await page.getByRole('textbox', { name: /^Message/ }).focus()
  await expect(page.locator('html')).toHaveCSS(
    '--app-keyboard-accessory-inset',
    '4px',
  )
  await page.waitForTimeout(50)

  const geometry = await page.evaluate(() => {
    const shell = document.querySelector<HTMLElement>('.shell')!
    const timeline = document.querySelector<HTMLElement>('.timeline')!
    const newestRow = [
      ...document.querySelectorAll<HTMLElement>('.event-row'),
    ].find((row) => row.textContent?.includes('standalone newest message'))!
    const composer = document.querySelector<HTMLElement>(
      '.room-stream > .composer',
    )!
    const newestBox = newestRow.getBoundingClientRect()
    const timelineBox = timeline.getBoundingClientRect()
    return {
      bottomGap:
        shell.getBoundingClientRect().bottom -
        composer.getBoundingClientRect().bottom,
      newestBottom: newestBox.bottom,
      timelineBottom: timelineBox.bottom,
      shellHeight: shell.getBoundingClientRect().height,
    }
  })

  await expect(page.locator('.event-row', { hasText: newest })).toBeVisible()
  expect(geometry.bottomGap).toBeLessThanOrEqual(12)
  expect(geometry.newestBottom).toBeLessThanOrEqual(geometry.timelineBottom)
  expect(geometry.shellHeight).toBeGreaterThan(700)
})

test('narrow: delayed thread badges preserve the bottom-pinned newest message', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  const newest = 'newest with replies'
  const newestId = '$threaded-newest'
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            ...Array.from({ length: 8 }, (_, index) =>
              sparseEvent(
                `$thread-hydration-${index}`,
                `thread hydration ${index}`,
              ),
            ),
            sparseEvent(newestId, newest),
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.route(/\/rooms\/[^/]+\/threads$/, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 120))
    await route.fulfill({
      json: {
        data: [
          {
            root_event_id: newestId,
            reply_count: 3,
            latest_reply_event_id: '$reply-latest',
            latest_reply_ts: Date.now(),
          },
        ],
      },
    })
  })

  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')
  await expect(page.getByRole('button', { name: /3 replies/ })).toBeVisible()

  const geometry = await page.evaluate((body) => {
    const timeline = document.querySelector<HTMLElement>('.timeline')!
    const newestRow = [
      ...document.querySelectorAll<HTMLElement>('.event-row'),
    ].find((row) => row.textContent?.includes(body))!
    const timelineBox = timeline.getBoundingClientRect()
    const rowBox = newestRow.getBoundingClientRect()
    return {
      rowBottom: rowBox.bottom,
      timelineBottom: timelineBox.bottom,
      distanceFromBottom:
        timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight,
    }
  }, newest)

  expect(geometry.rowBottom).toBeLessThanOrEqual(geometry.timelineBottom)
  expect(geometry.distanceFromBottom).toBeLessThanOrEqual(2)
})

test('narrow: event timestamps do not wrap after mobile chrome settles', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')

  await expect(page.locator('.event-head time').first()).toBeVisible()
  await page.getByRole('textbox', { name: /^Message/ }).fill('typing')

  const timestamps = await page.locator('.event-head time').evaluateAll((els) =>
    els.map((el) => {
      return {
        rectCount: el.getClientRects().length,
        text: el.textContent ?? '',
        whiteSpace: getComputedStyle(el).whiteSpace,
      }
    }),
  )
  const senders = await page
    .locator('.event-head .event-sender')
    .evaluateAll((els) => els.map((el) => el.getBoundingClientRect().width))

  expect(timestamps.length).toBeGreaterThan(0)
  for (const timestamp of timestamps) {
    expect(timestamp.text).toMatch(/^(?:1[0-2]|[1-9]):[0-5][0-9][ap]m$/)
    expect(timestamp.whiteSpace).toBe('nowrap')
    expect(timestamp.rectCount).toBe(1)
  }
  expect(senders.length).toBeGreaterThan(0)
  expect(senders.every((width) => width > 0)).toBe(true)
})

test('narrow: message actions fit as icon buttons', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')

  const body = `mobile action fit ${Date.now()}`
  await page.getByRole('textbox', { name: /^Message/ }).fill(body)
  await page.getByRole('button', { name: 'Send' }).click()

  const row = page.locator('.event-row', { hasText: body }).last()
  await expect(row).toBeVisible()
  await expect(row.getByRole('button', { name: 'Reply' })).toHaveCount(0)
  await row.locator('.event-body').click()

  await expect(row.getByRole('button', { name: 'Reply' })).toBeVisible()
  await expect(row.getByRole('button', { name: 'Thread' })).toBeVisible()
  await expect(row.getByRole('button', { name: 'React' })).toBeVisible()
  await expect(row.getByRole('button', { name: 'Edit' })).toBeVisible()
  await expect(row.getByRole('button', { name: 'Delete' })).toBeVisible()

  const initial = await mobileActionGeometry(row.locator('.event-actions'))
  expect(initial.right).toBeLessThanOrEqual(initial.viewportWidth)
  expect(initial.buttonCount).toBeGreaterThanOrEqual(5)
  expect(initial.iconCount).toBe(initial.buttonCount)
  expect(initial.visibleLabelCount).toBe(0)

  await row.getByRole('button', { name: 'Delete' }).click()
  await expect(
    row.getByRole('button', { name: 'Confirm delete' }),
  ).toBeVisible()
  await expect(row.getByRole('button', { name: 'Cancel' })).toBeVisible()

  const confirming = await mobileActionGeometry(row.locator('.event-actions'))
  expect(confirming.right).toBeLessThanOrEqual(confirming.viewportWidth)
  expect(confirming.iconCount).toBe(confirming.buttonCount)
  expect(confirming.visibleLabelCount).toBe(0)
})

test('narrow: thread badge opens on the first tap', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route(/\/rooms\/[^/]+\/threads$/, (route) =>
    route.fulfill({
      json: {
        data: [
          {
            root_event_id: '$seed-image:hs',
            reply_count: 2,
            latest_reply_event_id: '$thread-reply',
            latest_reply_ts: Date.now(),
          },
        ],
      },
    }),
  )
  await page.route(/\/threads\/[^/]+\/timeline$/, (route) =>
    route.fulfill({ json: { data: { events: [], next_cursor: null } } }),
  )

  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')

  const badge = page.getByRole('button', { name: /2 replies/ })
  await expect(badge).toBeVisible()
  const badgeRow = page.locator('.event-row', { has: badge })
  await expect(badgeRow.getByRole('button', { name: 'Reply' })).toHaveCount(0)
  await badge.click()

  await expect(
    page.getByRole('complementary', { name: 'Thread' }),
  ).toBeVisible()
})

test('narrow: unread-thread drawer lists hidden thread replies', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  const root = {
    account_id: '11111111-1111-4111-8111-111111111111',
    event_id: '$seed-image:hs',
    room_id: ROOM_ID,
    sender: '@bob:hs',
    state_key: null,
    origin_ts: 100,
    type: 'm.room.message',
    content: { msgtype: 'm.text', body: 'root body' },
    body: 'root body',
    relates_to: null,
    redacted: false,
    redaction_event_id: null,
    sender_trust: null,
    edited: false,
    edit_count: 0,
    latest_edit_ts: null,
    reactions: null,
  }
  const reply = {
    ...root,
    event_id: '$thread-reply:hs',
    origin_ts: 200,
    body: 'hidden reply',
    content: { msgtype: 'm.text', body: 'hidden reply' },
    relates_to: { rel_type: 'm.thread', event_id: '$seed-image:hs' },
  }
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: { data: { events: [root, reply], next_cursor: null } },
    }),
  )
  await page.route(/\/rooms\/[^/]+\/threads$/, (route) =>
    route.fulfill({
      json: {
        data: [
          {
            root_event_id: '$seed-image:hs',
            reply_count: 1,
            latest_reply_event_id: '$thread-reply:hs',
            latest_reply_ts: 200,
          },
        ],
      },
    }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({ json: { data: { events: [reply], next_cursor: null } } }),
  )
  await page.route(/\/devices\/[^/]+\/state\/[^/]+(?:\?.*)?$/, (route) => {
    const namespace = new URL(route.request().url()).pathname.split('/').pop()
    route.fulfill({
      json: {
        data: {
          namespace,
          entries:
            namespace === 'read_markers'
              ? {
                  [ROOM_ID]: {
                    value: { event_id: '$before:hs', origin_ts: 150 },
                    device_id: 'e2e',
                    updated_at: '2026-01-01T00:00:00Z',
                  },
                }
              : {},
        },
      },
    })
  })

  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')
  const button = page.getByRole('button', { name: 'Unread threads, 1' })
  await expect(button).toBeVisible()
  await button.click()

  const panel = page.getByRole('dialog', { name: 'Unread threads' })
  await expect(panel).toBeVisible()
  await expect(panel.getByText('E2E Room')).toBeVisible()
  await expect(panel.getByText('New thread activity')).toBeVisible()
  const box = await panel.boundingBox()
  expect(box?.x).toBeGreaterThanOrEqual(0)
  expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(390)
})

test('narrow: sparse room messages remain visible when composing', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: {
        data: {
          events: [sparseEvent('$sparse-room', 'short room')],
          next_cursor: null,
        },
      },
    }),
  )

  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')
  await page.getByRole('textbox', { name: /^Message/ }).focus()
  await page.evaluate(() => {
    document.documentElement.style.setProperty('--app-viewport-height', '520px')
  })
  await page.waitForTimeout(50)

  const geometry = await page.evaluate(() => {
    const timeline = document.querySelector<HTMLElement>('.timeline')!
    const row = document.querySelector<HTMLElement>('.event-row')!
    const composer = document.querySelector<HTMLElement>(
      '.room-stream > .composer',
    )!
    const timelineBox = timeline.getBoundingClientRect()
    const rowBox = row.getBoundingClientRect()
    return {
      rowTop: rowBox.top,
      rowBottom: rowBox.bottom,
      timelineTop: timelineBox.top,
      timelineBottom: timelineBox.bottom,
      rowToComposer: composer.getBoundingClientRect().top - rowBox.bottom,
    }
  })

  expect(geometry.rowTop).toBeGreaterThanOrEqual(geometry.timelineTop)
  expect(geometry.rowBottom).toBeLessThanOrEqual(geometry.timelineBottom)
  expect(geometry.rowToComposer).toBeLessThan(80)
})

test('narrow: sparse thread messages remain visible when composing', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: {
        data: {
          events: [sparseEvent('$sparse-thread', 'short thread')],
          next_cursor: null,
        },
      },
    }),
  )

  await page.goto(`${ROOM_URL}?thread=%24seed-image%3Ahs`)
  await expect(
    page.getByRole('complementary', { name: 'Thread' }),
  ).toBeVisible()
  await expect(page.locator('.thread-list .event-row')).toBeVisible()

  const grouped = await page.evaluate(() => {
    const root = document.querySelector<HTMLElement>('.thread-root')!
    const row = document.querySelector<HTMLElement>('.thread-list .event-row')!
    return row.getBoundingClientRect().top - root.getBoundingClientRect().bottom
  })
  expect(grouped).toBeLessThan(24)

  await page.getByRole('textbox', { name: 'Reply in thread' }).focus()
  await page.evaluate(() => {
    document.documentElement.style.setProperty('--app-viewport-height', '520px')
  })
  await page.waitForTimeout(50)

  const geometry = await page.evaluate(() => {
    const timeline = document.querySelector<HTMLElement>('.thread-timeline')!
    const root = document.querySelector<HTMLElement>('.thread-root')!
    const row = document.querySelector<HTMLElement>('.thread-list .event-row')!
    const composer = document.querySelector<HTMLElement>(
      '.thread-panel > .composer',
    )!
    const timelineBox = timeline.getBoundingClientRect()
    const rootBox = root.getBoundingClientRect()
    const rowBox = row.getBoundingClientRect()
    return {
      rootTop: rootBox.top,
      rootBottom: rootBox.bottom,
      rowTop: rowBox.top,
      rowBottom: rowBox.bottom,
      timelineTop: timelineBox.top,
      timelineBottom: timelineBox.bottom,
      rootToRow: rowBox.top - rootBox.bottom,
      composerTop: composer.getBoundingClientRect().top,
    }
  })

  expect(geometry.rootTop).toBeGreaterThanOrEqual(0)
  expect(geometry.rootToRow).toBeLessThan(24)
  expect(geometry.rowTop).toBeGreaterThanOrEqual(geometry.timelineTop)
  expect(geometry.rowBottom).toBeLessThanOrEqual(geometry.timelineBottom)
  expect(geometry.composerTop).toBeGreaterThan(geometry.rowBottom)
})

// A regression guard: the topbar had no `flex-wrap`, and `.room-stream`'s
// min-width once pushed a phone-width document sideways.
for (const viewport of [320, 640, 880, 1024, 1400, 1920]) {
  test(`no horizontal overflow at ${viewport}px`, async ({ page }) => {
    await signIn(page)
    await page.setViewportSize({ width: viewport, height: 900 })
    await page.goto(ROOM_URL)
    await expect(page.getByRole('status')).toHaveText('Live')

    const overflow = await page.evaluate(() => {
      const el = document.documentElement
      return el.scrollWidth - el.clientWidth
    })
    expect(overflow).toBeLessThanOrEqual(0)
  })
}

async function mobileActionGeometry(actions: Locator) {
  return actions.evaluate((el) => {
    const box = el.getBoundingClientRect()
    const buttons = [...el.querySelectorAll('button')]
    const icons = [...el.querySelectorAll('.event-action-icon')].filter(
      (icon) => getComputedStyle(icon).display !== 'none',
    )
    const visibleLabels = [
      ...el.querySelectorAll<HTMLElement>('.event-action-label'),
    ].filter((label) => label.getBoundingClientRect().width > 1)
    return {
      buttonCount: buttons.length,
      iconCount: icons.length,
      right: box.right,
      viewportWidth: window.innerWidth,
      visibleLabelCount: visibleLabels.length,
    }
  })
}

function sparseEvent(eventId: string, body: string) {
  return {
    account_id: '@alice:hs',
    event_id: eventId,
    room_id: '!room:hs',
    sender: '@alice:hs',
    state_key: null,
    origin_ts: Date.now(),
    type: 'm.room.message',
    content: { msgtype: 'm.text', body },
    body,
    relates_to: null,
    redacted: false,
    redaction_event_id: null,
    sender_trust: null,
    edited: false,
    edit_count: 0,
    latest_edit_ts: null,
    reactions: null,
  }
}
