import { expect, type Page } from '@playwright/test'

export const ACCOUNT_ID = '11111111-1111-4111-8111-111111111111'
export const ROOM_ID = '!room:hs'
export const ROOM_URL = `/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`

/**
 * The oldest room in the fixture, and so the last row under the default
 * recent-first sort — where "previous room" wraps to from the first row. It is
 * unnamed, so its rendered title is its room id.
 */
export const ACCOUNT_ID_2 = '22222222-2222-4222-8222-222222222222'
export const LAST_ROOM_ID = '!vQZGkFhTnLxWpRdCmYbA:hs'
export const LAST_ROOM_URL = `/${ACCOUNT_ID_2}/rooms/${encodeURIComponent(LAST_ROOM_ID)}`

/** Seed the token before app scripts run, so the shell renders signed in. */
export async function signIn(page: Page): Promise<void> {
  await page.addInitScript(() =>
    localStorage.setItem('axon.token', 'e2e-token'),
  )
}

/**
 * Reload the document from *inside* the page, not through `page.reload()`.
 *
 * Use this wherever a test depends on the engine *classifying* the load as a
 * reload — anything reading Navigation Timing, directly or through `perf.ts`'s
 * `nav` field, and anything gated on `isReloadOrRestoreNavigation`. Playwright's
 * driver reload is reported as `navigate` by Navigation Timing under Firefox.
 * Measured on Firefox 151:
 *
 * | engine   | `page.reload()` | in-page `location.reload()` |
 * |----------|-----------------|-----------------------------|
 * | chromium | `reload`        | `reload`                    |
 * | firefox  | **`navigate`**  | `reload`                    |
 * | webkit   | `reload`        | `reload`                    |
 *
 * So the driver is the outlier, not the engine, and nothing a user does reaches
 * it. An in-page reload is engine-neutral and the closer stand-in for Ctrl-R.
 *
 * `page.reload()` is still correct — and stays — wherever a test only needs a
 * fresh document and asserts nothing about how the load was classified.
 *
 * The `setTimeout` lets `evaluate` return before the execution context is torn
 * down; calling `location.reload()` synchronously rejects it instead.
 */
export async function reloadFromInsidePage(page: Page): Promise<void> {
  await Promise.all([
    page.waitForEvent('load'),
    page.evaluate(() => {
      window.setTimeout(() => location.reload(), 0)
    }),
  ])
}

/** A signed-in tab at the room, wide enough for two panes, socket up. */
export async function openRoom(page: Page): Promise<void> {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status')).toHaveText('Live')
}

/**
 * A stable descriptor of `document.activeElement` — real focus is what the
 * keyboard layer moves around (ADR 0078), so assertions read it directly.
 */
export function active(page: Page): Promise<string> {
  return page.evaluate(() => {
    const el = document.activeElement
    if (el === null) {
      return 'none'
    }
    const label = el.getAttribute('aria-label')
    const cls = el.className === '' ? '' : `.${el.className}`
    return `${el.tagName}${label === null ? '' : `[${label}]`}${cls}`
  })
}

/**
 * Whether an element is absent, present-but-`display:none`, or rendered. The
 * two-pane layout (ADR 0062) keeps both panes mounted and hides one with CSS,
 * so "not visible" and "not in the DOM" are different claims.
 */
export function shown(
  page: Page,
  selector: string,
): Promise<'absent' | 'hidden' | 'visible'> {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel)
    if (el === null) {
      return 'absent' as const
    }
    return getComputedStyle(el).display === 'none'
      ? ('hidden' as const)
      : ('visible' as const)
  }, selector)
}

/** The rendered width of `selector`, or null when it is absent. */
export function width(page: Page, selector: string): Promise<number | null> {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel)
    return el === null ? null : el.getBoundingClientRect().width
  }, selector)
}

/**
 * Assert the center of a visible content pane is actually hit-testable.
 * `toBeVisible()` only proves CSS visibility; mobile regressions here have
 * repeatedly left the timeline rendered but covered by a stale overlay.
 */
export async function expectPaneCenterUncovered(
  page: Page,
  selector: string,
): Promise<void> {
  await expect
    .poll(
      () =>
        page.evaluate((sel) => {
          const pane = document.querySelector<HTMLElement>(sel)
          if (pane === null) {
            return { ok: false, reason: 'missing' }
          }
          const box = pane.getBoundingClientRect()
          if (box.width <= 0 || box.height <= 0) {
            return { ok: false, reason: 'empty' }
          }
          const x = box.left + box.width / 2
          const y = box.top + box.height / 2
          const top = document.elementFromPoint(x, y)
          return {
            ok: top === pane || (top instanceof Element && pane.contains(top)),
            reason:
              top instanceof Element
                ? `${top.tagName.toLowerCase()}${top.className === '' ? '' : `.${top.className}`}`
                : 'none',
          }
        }, selector),
      { message: `${selector} center should not be covered by another pane` },
    )
    .toMatchObject({ ok: true })
}
