import { expect, test, type Page } from '@playwright/test'
import {
  BEAT,
  LONG,
  dwell,
  hover,
  personaName,
  point,
  resetRoom,
  roomUrl,
  spaceName,
  stage,
  typeInto,
} from './demo'

/**
 * The desktop web demo (ADR 0086 phase 3).
 *
 * One `test()` per scene, because Playwright records one video per test: the
 * output is a set of per-scene clips an editor can cut, and `docs/demo-coverage.md`
 * names scenes exactly this way. There is deliberately no `tour` — the TUI has
 * one only because a human screen-records a single continuous terminal session.
 *
 * Scenes are coupled to the corpus prose: the strings below are substrings of
 * message bodies in `smoke/local-stack/corpus/demo.toml`. Rewording a message
 * there fails the scene loudly, which is the intended failure mode — a demo
 * that silently stops demonstrating anything is the thing the coverage table
 * exists to prevent.
 */

/** The rendered timeline rows, which is what every content assertion reads. */
const rows = (page: Page) => page.locator('.event-list [data-event-id]')

/**
 * The room-list filter chips. Scoped to their own group because the `All
 * rooms` chip and the spaces rail's `All rooms` button share an accessible
 * name — an unscoped `getByRole` matches both and resolves to neither.
 */
const chip = (page: Page, name: string) =>
  page.locator('.filter-group').getByRole('button', { name, exact: true })

/**
 * The number of matches the *server* found, read off the overlay's status
 * line. `a.search-hit` counts only what is currently paged in.
 */
function resultTotal(page: Page): Promise<number> {
  // Deliberately total and non-throwing: the same line reads "Searching…"
  // while a request is in flight, and every caller is inside an `expect.poll`
  // that already knows how to wait. An assertion in here instead would be a
  // second, shorter timeout nested inside the first — which is how this
  // returned "Searching…" as a hard failure on a real backend.
  return page
    .locator('.search-status')
    .innerText()
    .then((text) => {
      const match = /^(\d+)/.exec(text.trim())
      return match === null ? Number.NaN : Number(match[1])
    })
}

/** `resultTotal`, once the search behind it has actually landed. */
async function settledTotal(page: Page): Promise<number> {
  await expect
    .poll(() => resultTotal(page), { message: 'waiting for a result count' })
    .toBeGreaterThanOrEqual(0)
  return resultTotal(page)
}

/** A spaces-rail entry, scoped for the same reason. */
const space = (page: Page, name: string) =>
  page.locator('.space-picker').getByRole('button', { name, exact: true })

test('rooms — the room list sorts, filters, and derives a DM title', async ({
  page,
}) => {
  // Staged inside a room rather than at `/`. At the index the right-hand pane
  // is the "Add a Room" form, which is most of the frame and none of the
  // point; beside an open timeline, the list is what the eye goes to and the
  // frame is the app as it is actually used.
  await stage(page, await roomUrl('trail-work'))

  const list = page.locator('.room-list-pane')
  await expect(list).toContainText('general')
  await expect(list).toContainText('trail-work')
  await expect(list).toContainText('gear-swap')
  await dwell(page, LONG)

  // Each narrowing step is proved by what *left* the list. `gear-swap` appears
  // nowhere else on screen, which makes it a clean witness for a filter having
  // actually applied — rather than for the thing being looked for having
  // already been there.
  await point(page, chip(page, 'Groups'))
  await expect(list).toContainText('trail-work')
  await dwell(page, BEAT)

  await point(page, chip(page, 'DMs'))
  // A DM has no name of its own; both clients derive its title from the other
  // member (web ported this from clients/tui/src/app/rooms.rs).
  await expect(list).toContainText(personaName('maya'))
  await expect(list).not.toContainText('gear-swap')
  await dwell(page, LONG)

  await point(page, chip(page, 'All rooms'))
  await expect(list).toContainText('gear-swap')
  await dwell(page, BEAT)

  // The live name filter narrows as it is typed (ADR 0042).
  const filter = page.getByRole('searchbox', { name: 'Filter by name' })
  await typeInto(filter, 'trail')
  await expect(list).toContainText('trail-work')
  await expect(list).not.toContainText('gear-swap')
  await dwell(page, LONG)

  await point(page, page.getByRole('button', { name: 'Clear room filter' }))
  await expect(list).toContainText('gear-swap')
  await dwell(page, BEAT)

  // Sort (ADR 0042). Asserting that the *first* row changed is what proves the
  // order changed — the set of rooms is identical either way, so any assertion
  // about membership is satisfied before the click.
  // Identified by href, not by rendered text: a row carries a relative
  // timestamp that ticks over ("1m" → "2m") while the scene is running, so
  // comparing whole-row text reports a reorder that never happened.
  const first = list.locator('a.room-link').first()
  const byRecent = await first.getAttribute('href')
  await list.locator('.sort-select select').first().selectOption('az')
  await expect(first).not.toHaveAttribute('href', byRecent ?? '')
  await dwell(page, LONG)

  await list.locator('.sort-select select').first().selectOption('recent')
  await expect(first).toHaveAttribute('href', byRecent ?? '')
  await dwell(page, BEAT)
})

test('spaces — the rail filters the room list by space hierarchy', async ({
  page,
}) => {
  await stage(page, await roomUrl('trail-work'))

  const list = page.locator('.room-list-pane')
  await expect(page.locator('.space-picker')).toBeVisible()
  await dwell(page, BEAT)

  // Ridgeline holds the trail rooms; the watershed council's rooms are not its
  // children, so `water-quality` leaving the list is what proves the filter.
  await point(page, space(page, spaceName('ridgeline')))
  await expect(list).toContainText('trail-work')
  await expect(list).not.toContainText('water-quality')
  await dwell(page, LONG)

  await point(page, space(page, spaceName('watershed')))
  await expect(list).toContainText('water-quality')
  await expect(list).not.toContainText('trail-work')
  await dwell(page, LONG)

  await point(page, space(page, 'All rooms'))
  await expect(list).toContainText('trail-work')
  await expect(list).toContainText('water-quality')
  await dwell(page, BEAT)
})

test('timeline — formatted messages, replies, reactions, day separators', async ({
  page,
}) => {
  await stage(page, await roomUrl('general'))

  await expect(rows(page).first()).toBeVisible()
  await expect(page.locator('.day-separator').first()).toBeVisible()
  await dwell(page, LONG)

  // A formatted (HTML) message with a real link.
  const parking = page.locator('.event-row', { hasText: 'Parking note' })
  await parking.scrollIntoViewIfNeeded()
  await expect(parking.locator('strong')).toHaveText('Parking note:')
  await expect(parking.locator('a')).toContainText('Ferndale trailhead')
  await dwell(page, LONG)

  // Seeded reaction badges, and the sender tooltip behind one of them.
  const laughed = page.locator('.event-row', {
    hasText: 'Twenty optimistic minutes',
  })
  await laughed.scrollIntoViewIfNeeded()
  const reaction = laughed.locator('.reaction-chip-wrap').first()
  await expect(reaction).toBeVisible()
  await hover(page, reaction)
  await dwell(page, LONG)

  // A reply renders its quoted context above the message.
  const reply = page.locator('.event-row', { hasText: "We're saved" })
  await reply.scrollIntoViewIfNeeded()
  await expect(reply.locator('.reply-context')).toContainText('rock bar')
  await dwell(page, LONG)
})

test('threads — an edit, a redaction, and the thread panel', async ({
  page,
}) => {
  await stage(page, await roomUrl('trail-work'))
  await expect(rows(page).first()).toBeVisible()

  // An `m.replace` applied in place, with the marker that says so.
  const edited = page.locator('.event-row', { hasText: 'rock bars Saturday' })
  await edited.scrollIntoViewIfNeeded()
  await expect(edited).toContainText('loose heads')
  await expect(edited.locator('.edited-marker')).toBeVisible()
  await dwell(page, LONG)

  // A real removal, not a hidden row: the tombstone stays in the timeline.
  // There is no `redacted` class to assert on — the row is ordinary and its
  // *body* is replaced by the placeholder, which is the thing on screen.
  await expect(
    page.locator('.event-row', { hasText: 'message deleted' }).first(),
  ).toBeVisible()
  await dwell(page, BEAT)

  // The planning thread. The badge counts replies the room timeline does not
  // show; opening it is what makes them readable.
  const badge = page.locator('.thread-badge').first()
  await expect(badge).toContainText('replies')
  await point(page, badge)

  const panel = page.getByRole('complementary', { name: 'Thread' })
  await expect(panel).toBeVisible()
  await expect(panel).toContainText('rock bars')
  await expect(panel).toContainText('Final headcount is nine')
  await dwell(page, LONG)

  // Leave the room as the scene found it.
  await page.keyboard.press('Escape')
  await expect(panel).toBeHidden()
  await dwell(page, BEAT)
})

test('media — an adjacency-grouped gallery and the lightbox', async ({
  page,
}) => {
  await stage(page, await roomUrl('trip-photos'))
  await expect(rows(page).first()).toBeVisible()

  // Four images from one sender, seconds apart, with nothing between: ADR
  // 0081's adjacency heuristic groups exactly that into one grid.
  const gallery = page.locator('.gallery-row').first()
  await gallery.scrollIntoViewIfNeeded()
  await expect(gallery.locator('.gallery-cell')).toHaveCount(4)
  await dwell(page, LONG)

  // The near-miss below it — same sender, same day, four minutes later with a
  // message in between — stays a standalone row. That contrast is the point.
  await expect(page.locator('.media-figure').first()).toBeVisible()
  await dwell(page, BEAT)

  await point(page, gallery.locator('.gallery-cell').first())
  const lightbox = page.getByRole('dialog')
  await expect(lightbox).toBeVisible()
  await dwell(page, LONG)

  // Paging swaps the decoded bytes; the counter is what proves it moved.
  await expect(lightbox.getByRole('status')).toContainText('1/4')
  await point(page, page.locator('.lightbox-next'))
  await expect(lightbox.getByRole('status')).toContainText('2/4')
  await dwell(page, BEAT)
  await point(page, page.locator('.lightbox-next'))
  await expect(lightbox.getByRole('status')).toContainText('3/4')
  await dwell(page, BEAT)
  await point(page, page.locator('.lightbox-next'))
  await expect(lightbox.getByRole('status')).toContainText('4/4')
  await dwell(page, LONG)

  await page.keyboard.press('Escape')
  await expect(lightbox).toHaveCount(0)
  await dwell(page, BEAT)
})

test('search — scoped to a room, widened, then narrowed again', async ({
  page,
}) => {
  await stage(page, await roomUrl('general'))
  await expect(rows(page).first()).toBeVisible()

  // `/` never fires from the composer — it has to type a slash there — so park
  // focus on the page first (ADR 0066).
  await page.locator('.timeline').click({ position: { x: 20, y: 20 } })
  await page.keyboard.press('/')
  const dialog = page.getByRole('dialog', { name: 'Search messages' })
  await expect(dialog).toBeVisible()
  await dwell(page, BEAT)

  // "switchback" is seeded across three rooms, five senders and five weeks
  // precisely so scope changes visibly change the answer rather than returning
  // the same list three times.
  const query = page.getByLabel('Search query')
  await typeInto(query, 'switchback')
  await page.keyboard.press('Enter')

  const hits = page.locator('a.search-hit')
  await expect(hits.first()).toBeVisible()
  await expect(hits.first().locator('mark').first()).toContainText(
    /switchback/i,
  )
  // The point of invocation decides the default scope (ADR 0066): opened from
  // inside #general, this searched #general — not everything.
  const inRoom = await settledTotal(page)
  await dwell(page, LONG)

  // `all:true` widens past the open room. Read the server's total rather than
  // `hits.count()`: the overlay auto-pages on scroll, so the number of
  // rendered hits reports whichever page the list happens to be on.
  await query.fill('')
  await typeInto(query, 'switchback all:true')
  await page.keyboard.press('Enter')
  await expect
    .poll(() => resultTotal(page), {
      message: 'all:true should widen past the open room',
    })
    .toBeGreaterThan(inRoom)
  const everywhere = await settledTotal(page)
  await dwell(page, LONG)

  // And `room:` narrows again — against a real Tantivy index, so this is the
  // server's field filter and not a substring match standing in for one.
  // (`all:true` cannot combine with `room:`, so the query is retyped.)
  await query.fill('')
  await typeInto(query, 'switchback room:trip-photos')
  await page.keyboard.press('Enter')
  await expect
    .poll(() => resultTotal(page), {
      message: 'room: should narrow the results',
    })
    .toBeLessThan(everywhere)
  await dwell(page, LONG)

  // A hit is a deep link: it closes the overlay, opens the room, and reveals
  // and highlights the event.
  await point(page, hits.first())
  await expect(dialog).toHaveCount(0)
  await expect(page).toHaveURL(/\?event=/)
  await expect(page.locator('.event-row.highlighted')).toBeVisible()
  await dwell(page, LONG)
})

const SENT = 'Reading it now — the turbidity section is the strongest part.'

test('send — a message and a reaction, both undone', async ({ page }) => {
  // Clear anything a previous, half-finished run left behind, before the
  // browser opens — see `resetRoom`.
  await resetRoom('dm-maya', 'turbidity section')
  await stage(page, await roomUrl('dm-maya'))
  await expect(rows(page).first()).toBeVisible()
  await dwell(page, BEAT)

  const composer = page.getByRole('textbox', { name: /^Message/ })
  await typeInto(composer, SENT, 40)
  await dwell(page, BEAT)
  await composer.press('Enter')

  // The optimistic echo reconciles against the confirmed event; waiting on the
  // pending class clearing is what proves the round trip, not the text
  // appearing — the text is on screen the moment it is typed.
  const sent = page.locator('.event-row', { hasText: 'turbidity section' })
  await expect(sent).toBeVisible()
  await expect(page.locator('.event-row.pending')).toHaveCount(0)
  await dwell(page, LONG)

  // React to Maya's last message, then withdraw it.
  const thanks = page.locator('.event-row', {
    hasText: 'Thank you for driving',
  })
  await hover(page, thanks)
  await point(page, thanks.getByRole('button', { name: 'React' }))
  const picker = page.getByRole('group', { name: 'React with' })
  await expect(picker).toBeVisible()
  await point(page, picker.getByRole('button').first())
  await expect(thanks.locator('.reaction-chip-wrap')).toHaveCount(1)
  await dwell(page, LONG)

  // Undo, so the scene leaves the world as it found it: a react-only scene
  // passes vacuously on its second run, on the badge the first run left behind.
  await point(page, thanks.locator('.reaction-chip-wrap button').first())
  await expect(thanks.locator('.reaction-chip-wrap')).toHaveCount(0)

  await hover(page, sent)
  await point(page, sent.getByRole('button', { name: 'Delete', exact: true }))
  await point(page, sent.getByRole('button', { name: 'Confirm delete' }))
  await expect(
    page.locator('.event-row', { hasText: 'turbidity section' }),
  ).toHaveCount(0)
  await dwell(page, BEAT)
})

test('shortcuts — the keyboard map', async ({ page }) => {
  await stage(page, await roomUrl('general'))
  await expect(rows(page).first()).toBeVisible()

  await page.locator('.timeline').click({ position: { x: 20, y: 20 } })
  await page.keyboard.press('?')
  const help = page.getByRole('dialog', { name: 'Help' })
  await expect(help).toBeVisible()
  await expect(help).toContainText('Search messages')
  await dwell(page, LONG)

  await help.locator('.shortcut-list').last().scrollIntoViewIfNeeded()
  await dwell(page, LONG)

  await page.keyboard.press('Escape')
  await expect(help).toHaveCount(0)
  await dwell(page, BEAT)
})
