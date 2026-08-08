import { expect, test } from '@playwright/test'
import {
  active,
  LAST_ROOM_URL,
  openRoom,
  ROOM_URL,
  shortcutForProject,
  signIn,
} from './helpers'

/**
 * Keyboard shortcuts (ADR 0078). These need a real browser: the chords carry
 * modifiers a synthetic jsdom event never reproduces faithfully, focus moves
 * through elements CSS can hide, and Preact's after-paint effect flush — the
 * source of the stale-closure bug the Ctrl-B test guards — has no jsdom analogue.
 */
test.describe.configure({ mode: 'serial' })

test('Ctrl-K focuses the room filter, even from the composer', async ({
  page,
}) => {
  await openRoom(page)
  await page.getByRole('textbox', { name: /^Message/ }).focus()
  await expect.poll(() => active(page)).toContain('TEXTAREA')

  await page.keyboard.press('Control+k')

  await expect.poll(() => active(page)).toContain('Filter by name')
})

test('Ctrl-K un-collapses the sidebar before focusing it', async ({ page }) => {
  await openRoom(page)
  await page.getByRole('button', { name: 'Hide rooms' }).click()
  await expect(page.locator('.sidebar')).toBeHidden()

  await page.keyboard.press('Control+k')

  // focus() on a display:none element is a silent no-op, so the un-collapse
  // has to be styled before the focus lands.
  await expect(page.locator('.sidebar')).toBeVisible()
  await expect.poll(() => active(page)).toContain('Filter by name')
})

test('space shortcuts toggle the rail and cycle the visible space order', async ({
  page,
}) => {
  await openRoom(page)
  const rail = page.locator('.space-picker')

  await page.keyboard.press('Control+Alt+S')
  await expect(rail).toBeHidden()
  await page.keyboard.press('Control+Alt+S')
  await expect(rail).toBeVisible()

  await page.keyboard.press('Control+Alt+]')
  await expect(
    page.getByRole('button', { name: 'E2E Space One' }),
  ).toHaveAttribute('aria-pressed', 'true')
  await page.keyboard.press('Control+Alt+]')
  await expect(
    page.getByRole('button', { name: 'E2E Space Two' }),
  ).toHaveAttribute('aria-pressed', 'true')
  await page.keyboard.press('Control+Alt+[')
  await expect(
    page.getByRole('button', { name: 'E2E Space One' }),
  ).toHaveAttribute('aria-pressed', 'true')

  // This serial suite shares a page; restore the all-rooms view so room
  // stepping tests below retain their intended fixture ordering.
  const allSpaces = rail.getByRole('button', { name: 'All rooms' })
  await allSpaces.click()
  await expect(allSpaces).toHaveAttribute('aria-pressed', 'true')
})

test('arrows rove the room list; Enter opens; Escape returns to the composer', async ({
  page,
}) => {
  await openRoom(page)
  await page.keyboard.press('Control+k')
  await expect.poll(() => active(page)).toContain('Filter by name')

  await page.keyboard.press('ArrowDown')
  await expect.poll(() => active(page)).toContain('room-link')

  // Up off the top returns to where the sequence began.
  await page.keyboard.press('ArrowUp')
  await expect.poll(() => active(page)).toContain('Filter by name')

  await page.keyboard.press('Escape')
  await expect.poll(() => active(page)).toContain('TEXTAREA')

  // Enter activates the roved anchor natively.
  await page.keyboard.press('Control+k')
  await expect.poll(() => active(page)).toContain('Filter by name')
  await page.keyboard.press('ArrowDown')
  await expect.poll(() => active(page)).toContain('room-link')
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/rooms\//)
})

test('Ctrl-Shift-Y cycles the filter in the TUI’s order', async ({ page }) => {
  await openRoom(page)
  const activeChip = () =>
    page.locator('.chip.active').getAttribute('aria-label')

  expect(await activeChip()).toBe('All rooms')
  for (const expected of [
    'DMs',
    'Groups',
    'Unread',
    'Favorites',
    'All rooms',
  ]) {
    await page.keyboard.press('Control+Shift+Y')
    expect(await activeChip()).toBe(expected)
  }
})

test('Ctrl-Shift-S cycles the sort, and it persists', async ({ page }) => {
  await openRoom(page)

  await page.keyboard.press('Control+Shift+S')
  await expect(page.getByLabel('Sort')).toHaveValue('oldest')
  await page.keyboard.press('Control+Shift+S')
  await expect(page.getByLabel('Sort')).toHaveValue('az')

  await page.reload()
  await expect(page.getByLabel('Sort')).toHaveValue('az')
})

test('Ctrl-B toggles the sidebar both ways', async ({ page }) => {
  await openRoom(page)
  await expect(page.locator('.sidebar')).toBeVisible()

  await page.keyboard.press('Control+b')
  await expect(page.locator('.sidebar')).toBeHidden()

  // The second press is the interesting one: a handler re-subscribed per
  // render would still be closed over `collapsed: false` here and re-collapse.
  await page.keyboard.press('Control+b')
  await expect(page.locator('.sidebar')).toBeVisible()
})

test('Ctrl-ArrowDown moves to a room from the index', async ({ page }) => {
  await openRoom(page)
  await page.goto('/')
  await expect(page.locator('a.room-link').first()).toBeVisible()

  await page.keyboard.press('Control+ArrowDown')

  await expect(page).toHaveURL(/\/rooms\//)
})

/**
 * Both directions, from the composer — where focus actually lives while
 * reading. Ctrl-↑ used to do nothing at all: the composer's bare-`ArrowUp`
 * edit-last binding claimed it and called `preventDefault()`, which hides the
 * chord from `useShortcuts`. Ctrl-↓ had no such rival and worked, which is the
 * asymmetry that surfaced the bug.
 */
test('Ctrl-Arrow steps rooms while the composer holds focus', async ({
  page,
}) => {
  await openRoom(page)
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.focus()
  await expect(composer).toHaveValue('')

  // `E2E Room` is the first row, so "previous" wraps to the last one.
  await page.keyboard.press('Control+ArrowUp')
  await expect(page).toHaveURL(LAST_ROOM_URL)
  // The edit banner must not have opened behind the room change.
  await expect(page.locator('.composer-banner')).toHaveCount(0)

  // Wait for `activeRoom` to catch up before stepping again: `stepRoom` reads
  // it, and RoomPage sets it in a post-paint effect. The URL — and even the
  // room heading — lands before that, so waiting on either would step from the
  // stale index. The sidebar's `aria-current` is rendered *from* `activeRoom`,
  // so it is the one thing that cannot be ahead of it.
  await expect(
    page.locator('a.room-link[aria-current="page"]'),
  ).toHaveAttribute('href', LAST_ROOM_URL)

  await page.keyboard.press('Control+ArrowDown')
  await expect(page).toHaveURL(ROOM_URL)
})

/** The room list's own arrow roving must not swallow the chord either. */
test('Ctrl-Arrow steps rooms while the room list holds focus', async ({
  page,
}) => {
  await openRoom(page)
  await page.keyboard.press('Control+k')
  await expect(page.locator('input.name-filter')).toBeFocused()
  await page.keyboard.press('ArrowDown')
  await expect.poll(() => active(page)).toContain('room-link')

  await page.keyboard.press('Control+ArrowDown')
  await expect(page).not.toHaveURL(ROOM_URL)
})

/**
 * Stepping rooms from the composer must not cost the user the caret. The
 * composer is keyed by room id, so the destination mounts a *new* textarea —
 * focus has to be re-taken after that mount, not before.
 */
test('Ctrl-Arrow keeps focus in the composer it started in', async ({
  page,
}) => {
  await openRoom(page)
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.focus()
  await expect(composer).toBeFocused()

  await page.keyboard.press('Control+ArrowDown')
  await expect(page).toHaveURL(/!long%3Ahs/)

  // The focused element is the *destination* room's composer, not the one we
  // left: its placeholder names the room it sends to.
  const focused = page.locator('textarea:focus')
  await expect(focused).toHaveAttribute(
    'aria-label',
    'Message A Room With A Name Far Too Long For The Sidebar',
  )

  // The caret is live: keys land in that textarea. Measured as a delta, since
  // a draft persisted by an earlier run would prefill it.
  const before = await focused.inputValue()
  await page.keyboard.type('hello')
  await expect(focused).toHaveValue(`${before}hello`)
})

/** …but stepping from the room list leaves the keyboard in the room list. */
test('Ctrl-Arrow from the room list does not steal focus to the composer', async ({
  page,
}) => {
  await openRoom(page)
  await page.keyboard.press('Control+k')
  await expect(page.locator('input.name-filter')).toBeFocused()
  await page.keyboard.press('ArrowDown')
  await expect.poll(() => active(page)).toContain('room-link')

  await page.keyboard.press('Control+ArrowDown')
  await expect(page).not.toHaveURL(ROOM_URL)
  await expect.poll(() => active(page)).toContain('room-link')
})

test('? opens the help; Escape closes it; typing ? does not open it', async ({
  page,
}) => {
  await openRoom(page)
  const dialog = page.getByRole('dialog', { name: 'Help' })

  await page.keyboard.press('?')
  await expect(dialog).toBeVisible()
  // The popup renders the SHORTCUTS table, so it lists what is actually bound.
  await expect(dialog).toContainText('Ctrl-K')
  await expect(dialog).toContainText('Cycle filter')

  await page.keyboard.press('Escape')
  await expect(dialog).toBeHidden()

  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.click()
  await composer.pressSequentially('why? ')
  await expect(dialog).toBeHidden()
  await expect(composer).toHaveValue('why? ')
})

test('command descriptions stay aligned when command labels are long', async ({
  page,
}) => {
  await page.setViewportSize({ width: 820, height: 1180 })
  await openRoom(page)

  await page.keyboard.press('?')
  const commandSection = page.locator('.shortcut-group').filter({
    has: page.getByRole('heading', { name: 'Commands' }),
  })
  await expect(
    commandSection.getByText('Send hidden spoiler text'),
  ).toBeVisible()
  await expect(
    commandSection.getByText('Set room-list sort order'),
  ).toBeVisible()

  const lefts = await commandSection
    .locator('dd')
    .evaluateAll((nodes) =>
      nodes.map((node) => node.getBoundingClientRect().left),
    )
  const min = Math.min(...lefts)
  const max = Math.max(...lefts)
  expect(max - min).toBeLessThan(1)
})

test('ArrowUp on an empty composer edits your last message', async ({
  page,
}) => {
  await openRoom(page)
  const composer = page.getByRole('textbox', { name: /^Message/ })

  await composer.click()
  await composer.fill('my last words')
  await composer.press('Enter')
  await expect(
    page.locator('.event-row .body-text', { hasText: 'my last words' }).last(),
  ).toBeVisible()
  await expect(composer).toHaveValue('')

  await composer.press('ArrowUp')

  await expect(page.getByText('Editing')).toBeVisible()
  await expect(page.getByRole('textbox', { name: /^Message/ })).toHaveValue(
    'my last words',
  )

  // Escape cancels the banner; the composer claims it, nothing else acts.
  await page.keyboard.press('Escape')
  await expect(page.getByText('Editing')).toBeHidden()
})

test('Escape closes the thread panel, then focuses the composer', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await expect(page.locator('.thread-panel')).toBeVisible()

  await page.keyboard.press('Escape')

  await expect(page.locator('.thread-panel')).toBeHidden()
  await expect(page).not.toHaveURL(/thread=/)
  await expect.poll(() => active(page)).toContain('TEXTAREA')
})

test('Ctrl-/ opens the help from the composer, where ? cannot', async ({
  page,
}) => {
  await openRoom(page)
  const composer = page.getByRole('textbox', { name: /^Message/ })
  const dialog = page.getByRole('dialog', { name: 'Help' })
  await composer.click()

  // `?` has to reach the textarea as a character, so it types instead.
  await composer.pressSequentially('?')
  await expect(dialog).toBeHidden()
  await expect(composer).toHaveValue('?')

  await page.keyboard.press('Control+/')
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('? or Ctrl-/')
})

test('the ? button opens the help without touching the keyboard', async ({
  page,
}) => {
  await openRoom(page)
  const help = page.getByRole('button', { name: 'Keyboard shortcuts' })
  // The tooltip gained its slash-command hint in #88; the unit test at
  // `app.test.tsx` was updated with it and this lane was not.
  await expect(help).toHaveAttribute(
    'title',
    'Keyboard shortcuts (/help; ? or Ctrl-/)',
  )

  await help.click()

  await expect(page.getByRole('dialog', { name: 'Help' })).toBeVisible()
})

test('the room controls carry their chords as tooltips', async ({
  page,
}, testInfo) => {
  await openRoom(page)

  const filterShortcut = shortcutForProject(
    testInfo.project.name,
    'Ctrl-K',
    '⌘-K',
  )
  const sortShortcut = shortcutForProject(
    testInfo.project.name,
    'Ctrl-Shift-S',
    '⌘-Shift-S',
  )
  const roomsShortcut = shortcutForProject(
    testInfo.project.name,
    'Ctrl-B',
    '⌘-B',
  )

  await expect(page.getByLabel('Filter by name')).toHaveAttribute(
    'title',
    `Filter rooms by name (${filterShortcut})`,
  )
  await expect(page.getByLabel('Sort')).toHaveAttribute(
    'title',
    `Cycle sort order (${sortShortcut})`,
  )
  await expect(
    page.getByRole('button', { name: 'Hide rooms' }),
  ).toHaveAttribute(
    'title',
    `Hide rooms (${roomsShortcut}); drag or use arrow keys to resize`,
  )
})
