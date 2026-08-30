import { expect, test, type Page } from '@playwright/test'
import {
  BEAT,
  LONG,
  dwell,
  resetRoom,
  roomUrl,
  spaceName,
  stage,
  tap,
  tapType,
} from './demo'

/**
 * The mobile web demo (ADR 0086 phase 3), recorded on an iPhone 13 device
 * descriptor.
 *
 * This is not the desktop take at a narrower width. Below the two-pane
 * breakpoint the layout collapses the room list and the spaces rail behind
 * navigation, so the scenes differ in kind: a desktop scene points at a room
 * already on screen, a mobile scene has to navigate to it and back. Everything
 * here therefore leads with the transition, which is also the thing ADR 0070
 * measures — and the only place a viewer can see what a room switch costs.
 *
 * Nothing about the corpus or the seeder is viewport-aware; both recordings
 * read the same seeded world through the same axon.
 */

const rows = (page: Page) => page.locator('.event-list [data-event-id]')

/** The single visible pane is decided by the route, so assert on both sides. */
async function expectTimelinePane(page: Page): Promise<void> {
  await expect(page.locator('.shell main')).toBeVisible()
  await expect(page.locator('.sidebar')).toBeHidden()
}

async function expectRoomListPane(page: Page): Promise<void> {
  await expect(page.locator('.sidebar')).toBeVisible()
  await expect(page.locator('.shell main')).toBeHidden()
}

test('rooms — one pane, chosen by the route', async ({ page }) => {
  await stage(page, '/')
  await expectRoomListPane(page)

  const list = page.locator('.room-list-pane')
  await expect(list).toContainText('general')
  await expect(list).toContainText('trail-work')
  await dwell(page, LONG)

  // The spaces rail is reachable on a phone too, and filters the same way.
  // Scoped to the rail: its `All rooms` entry and the room-list filter chip of
  // the same name would otherwise both match.
  const rail = page.locator('.space-picker')
  await tap(
    page,
    rail.getByRole('button', { name: spaceName('watershed'), exact: true }),
  )
  await expect(list).toContainText('water-quality')
  await expect(list).not.toContainText('trail-work')
  await dwell(page, LONG)

  await tap(page, rail.getByRole('button', { name: 'All rooms', exact: true }))
  await expect(list).toContainText('trail-work')
  await dwell(page, BEAT)

  // Tapping a room replaces the pane; Back brings the list straight back.
  await tap(page, list.locator('a.room-link', { hasText: 'trail-work' }))
  await expectTimelinePane(page)
  await expect(rows(page).first()).toBeVisible()
  await dwell(page, LONG)

  await page.goBack()
  await expectRoomListPane(page)
  await dwell(page, BEAT)
})

test('timeline — reading a room on a phone', async ({ page }) => {
  await stage(page, await roomUrl('general'))
  await expectTimelinePane(page)
  await expect(rows(page).first()).toBeVisible()
  await dwell(page, LONG)

  // Walk back through history a screen at a time. Not `page.mouse.wheel`:
  // mobile WebKit has no wheel, which is the right answer — a phone has no
  // wheel either. A smooth-behaviour `scrollBy` is what reads as scrolling on
  // video without pretending to be a gesture it is not.
  const timeline = page.locator('.timeline')
  for (let i = 0; i < 3; i += 1) {
    await timeline.evaluate((el) =>
      el.scrollBy({ top: -420, behavior: 'smooth' }),
    )
    await dwell(page, BEAT)
  }

  const parking = page.locator('.event-row', { hasText: 'Parking note' })
  await parking.scrollIntoViewIfNeeded()
  await expect(parking.locator('strong')).toHaveText('Parking note:')
  await dwell(page, LONG)

  // Room information is where a phone puts everything the desktop header has
  // room for inline.
  await tap(page, page.getByRole('button', { name: 'Open room information' }))
  const info = page.locator('.room-info-panel')
  await expect(info).toBeVisible()
  await expect(info).toContainText('Announcements')
  await dwell(page, LONG)

  await page.keyboard.press('Escape')
  await expect(info).toBeHidden()
  await dwell(page, BEAT)
})

test('threads — the thread drawer over the room', async ({ page }) => {
  await stage(page, await roomUrl('trail-work'))
  await expect(rows(page).first()).toBeVisible()

  const badge = page.locator('.thread-badge').first()
  await expect(badge).toContainText('replies')
  await dwell(page, BEAT)

  // On a phone the thread is a drawer stacked over the room, not a third pane.
  await tap(page, badge)
  const panel = page.getByRole('complementary', { name: 'Thread' })
  await expect(panel).toBeVisible()
  await expect(panel).toContainText('Final headcount is nine')
  await dwell(page, LONG)

  await page.keyboard.press('Escape')
  await expect(panel).toBeHidden()
  await dwell(page, BEAT)
})

test('media — galleries and the lightbox on a phone', async ({ page }) => {
  await stage(page, await roomUrl('trip-photos'))
  await expect(rows(page).first()).toBeVisible()

  const gallery = page.locator('.gallery-row').first()
  await gallery.scrollIntoViewIfNeeded()
  await expect(gallery.locator('.gallery-cell')).toHaveCount(4)
  await dwell(page, LONG)

  await tap(page, gallery.locator('.gallery-cell').first())
  const lightbox = page.getByRole('dialog')
  await expect(lightbox).toBeVisible()
  await expect(lightbox.getByRole('status')).toContainText('1/4')
  await dwell(page, LONG)

  // Page with the on-screen control rather than a swipe: a swipe gesture and
  // the scroller's own drag handling are hard to tell apart on video, and the
  // control is what a phone user actually reaches for one-handed.
  await tap(page, page.locator('.lightbox-next'))
  await expect(lightbox.getByRole('status')).toContainText('2/4')
  await dwell(page, BEAT)
  await tap(page, page.locator('.lightbox-next'))
  await expect(lightbox.getByRole('status')).toContainText('3/4')
  await dwell(page, LONG)

  await tap(page, lightbox.getByRole('button', { name: 'Close' }))
  await expect(lightbox).toHaveCount(0)
  await dwell(page, BEAT)
})

test('search — finding a message and landing on it', async ({ page }) => {
  await stage(page, await roomUrl('general'))
  await expect(rows(page).first()).toBeVisible()

  // No keyboard to press `/` on: the topbar button is the mobile route in.
  await tap(page, page.getByRole('button', { name: 'Search messages' }))
  const dialog = page.getByRole('dialog', { name: 'Search messages' })
  await expect(dialog).toBeVisible()
  await dwell(page, BEAT)

  // `all:true` because the default scope is the point of invocation (ADR
  // 0066) — opened from #general, an unqualified query would search #general,
  // and "turbidity" lives in #water-quality.
  const query = page.getByLabel('Search query')
  await tapType(query, 'turbidity all:true')
  await page.keyboard.press('Enter')

  const hits = page.locator('a.search-hit')
  await expect(hits.first()).toBeVisible()
  await expect(hits.first().locator('mark').first()).toContainText(/turbidity/i)
  await dwell(page, LONG)

  // The hit is a deep link into a room this phone was not in a moment ago.
  await tap(page, hits.first())
  await expect(dialog).toHaveCount(0)
  await expectTimelinePane(page)
  await expect(page.locator('.event-row.highlighted')).toBeVisible()
  await dwell(page, LONG)
})

test('send — a message and its undo, from a phone', async ({ page }) => {
  // A different room from the desktop send scene, on purpose: the undo is a
  // redaction, and a redaction leaves a permanent tombstone row. Sharing one
  // room would mean whichever take runs second opens on the other's
  // "message deleted".
  await resetRoom('gear-swap', 'On the bus now')
  await stage(page, await roomUrl('gear-swap'))
  await expect(rows(page).first()).toBeVisible()
  await dwell(page, BEAT)

  const body = 'On the bus now — I can bring the orange dry bag Saturday.'
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await tapType(composer, body, 45)
  await dwell(page, BEAT)
  // The Send button, not Enter: it is what a phone keyboard puts under a
  // thumb, and it is the only send affordance a touch-only user has.
  await tap(page, page.getByRole('button', { name: 'Send', exact: true }))

  const sent = page.locator('.event-row', { hasText: 'On the bus now' })
  await expect(sent).toBeVisible()
  await expect(page.locator('.event-row.pending')).toHaveCount(0)
  await dwell(page, LONG)

  // Message actions are hidden until the row is tapped at this width, and
  // then they are icon buttons rather than labelled ones — but the same
  // actions, and the scene still scripts its own undo.
  await tap(page, sent.locator('.event-body'))
  await expect(
    sent.getByRole('button', { name: 'Delete', exact: true }),
  ).toBeVisible()
  await dwell(page, BEAT)
  await tap(page, sent.getByRole('button', { name: 'Delete', exact: true }))
  await tap(page, sent.getByRole('button', { name: 'Confirm delete' }))
  await expect(
    page.locator('.event-row', { hasText: 'On the bus now' }),
  ).toHaveCount(0)
  await dwell(page, BEAT)
})
