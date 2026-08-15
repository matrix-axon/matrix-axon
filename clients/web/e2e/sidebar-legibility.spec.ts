import { expect, test, type Page } from '@playwright/test'
import { ROOM_URL, active, expectLive, signIn } from './helpers'

/**
 * The sidebar has to stay readable on the screens people actually use.
 *
 * The bug this guards against was invisible for two reasons. It needed a second
 * account — that is what puts a user id in the row and starves the name of
 * width — and it clipped the name *without* an ellipsis, so nothing in the DOM
 * said the text had been lost. Both are now asserted directly.
 *
 * Viewport sizes are CSS pixels, which is the only thing layout sees. Windows
 * display scaling and browser zoom change the device-pixel ratio and the number
 * of CSS pixels respectively; neither adds an axis worth testing beyond the
 * sizes below, and `deviceScaleFactor` cannot affect layout at all.
 */

/** Typical laptop widths, plus 1920x1200 at Windows' 125% scaling. */
const VIEWPORTS = [
  { name: '1280x720', width: 1280, height: 720 },
  { name: '1366x768', width: 1366, height: 768 },
  { name: '1536x864 (1920x1080 @125%)', width: 1536, height: 864 },
]

/** The narrowest a room name may be and still be worth reading. */
const MIN_NAME_PX = 8 * 16

/** WCAG relative-luminance contrast ratio between two `rgb(...)` colors. */
function contrast(a: string, b: string): number {
  const luminance = (color: string) => {
    const [r, g, b2] = color.match(/\d+/g)!.slice(0, 3).map(Number)
    const channel = (v: number) => {
      const s = v / 255
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
    }
    return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b2)
  }
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x)
  return (hi + 0.05) / (lo + 0.05)
}

async function openRooms(page: Page): Promise<void> {
  await signIn(page)
  await page.goto('/')
  await page.locator('a.room-link').first().waitFor()
}

for (const viewport of VIEWPORTS) {
  test(`room names are legible at ${viewport.name}`, async ({ page }) => {
    await page.setViewportSize(viewport)
    await openRooms(page)

    const names = await page.evaluate(() =>
      [...document.querySelectorAll<HTMLElement>('.room-name')].map((el) => ({
        text: el.textContent ?? '',
        width: el.clientWidth,
      })),
    )

    expect(names.length).toBeGreaterThan(2) // fixture has multiple accounts
    for (const name of names) {
      expect(name.text.length, 'every row names its room').toBeGreaterThan(0)
      expect(
        name.width,
        `"${name.text}" is only ${name.width}px wide`,
      ).toBeGreaterThanOrEqual(MIN_NAME_PX)
    }
  })

  test(`nothing in the sidebar is clipped without an ellipsis at ${viewport.name}`, async ({
    page,
  }) => {
    await page.setViewportSize(viewport)
    await openRooms(page)

    // Overflowing is fine — silently swallowing the overflow is not. Anything
    // wider than its box must say so with an ellipsis.
    const clipped = await page.evaluate(() =>
      [...document.querySelectorAll<HTMLElement>('.sidebar *')]
        .filter((el) => el.offsetParent !== null)
        .filter((el) => el.scrollWidth > el.clientWidth + 1)
        .filter((el) => getComputedStyle(el).textOverflow !== 'ellipsis')
        .map((el) => ({
          selector: `${el.tagName.toLowerCase()}.${el.className}`,
          text: (el.textContent ?? '').slice(0, 40),
          clientWidth: el.clientWidth,
          scrollWidth: el.scrollWidth,
        })),
    )

    expect(clipped, 'elements clipped with no ellipsis').toEqual([])
  })
}

test('a long room name ellipsizes rather than starving the badges', async ({
  page,
}) => {
  await page.setViewportSize(VIEWPORTS[2])
  await openRooms(page)

  const long = page
    .locator('.room-row', { hasText: 'A Room With A Name Far Too Long' })
    .locator('.room-name')
  await expect(long).toBeVisible()

  const box = await long.evaluate((el) => ({
    clientWidth: el.clientWidth,
    scrollWidth: el.scrollWidth,
    ellipsis: getComputedStyle(el).textOverflow,
  }))
  expect(box.scrollWidth).toBeGreaterThan(box.clientWidth) // it really is long
  expect(box.ellipsis).toBe('ellipsis')
  expect(box.clientWidth).toBeGreaterThanOrEqual(MIN_NAME_PX)
})

test('the unnamed room falls back to its room id, ellipsized', async ({
  page,
}) => {
  await page.setViewportSize(VIEWPORTS[2])
  await openRooms(page)

  const fallback = page.locator('.room-name', {
    hasText: '!vQZGkFhTnLxWpRdCmYbA',
  })
  await expect(fallback).toBeVisible()
  expect(
    await fallback.evaluate((el) => el.clientWidth),
  ).toBeGreaterThanOrEqual(MIN_NAME_PX)
})

test('the account label shortens to its localpart', async ({ page }) => {
  await page.setViewportSize(VIEWPORTS[2])
  await openRooms(page)

  // Rows carry `@me`, not `@me:hs` — the two localparts here are unambiguous.
  const metas = await page
    .locator('.room-title-meta, .room-meta')
    .evaluateAll((els) => els.map((el) => el.textContent ?? ''))
  expect(metas.some((m) => m.includes('@me ·'))).toBe(true)
  expect(metas.some((m) => m.includes('@other ·'))).toBe(true)
  expect(metas.some((m) => m.includes(':hs'))).toBe(false)

  // The dropdown still disambiguates with full ids.
  const options = await page
    .getByLabel('Account')
    .locator('option')
    .evaluateAll((els) => els.map((el) => el.textContent ?? ''))
  expect(options).toContain('@me:hs')
  expect(options).toContain('@other:hs')
})

/**
 * The open room has to be findable at a glance. Hover is `--bg-raised`, a
 * neutral lightness shift, and the selected tint sits at almost the same
 * luminance (~1.1:1) — it differs in *hue*. So the accent bar and the semibold
 * name are load-bearing, not decoration: hue alone would fail a color-blind
 * user. These assert all three cues, in both themes.
 */
for (const theme of ['light', 'dark'] as const) {
  test(`the open room is marked with an accent, a bar and weight (${theme})`, async ({
    page,
  }) => {
    await signIn(page)
    await page.setViewportSize(VIEWPORTS[2])
    await page.goto(ROOM_URL)
    await page.evaluate(
      (t) => document.documentElement.setAttribute('data-theme', t),
      theme,
    )
    const current = page.locator('a.room-link[aria-current="page"]')
    await expect(current).toHaveCount(1)

    const style = await page.evaluate(() => {
      const sel = document.querySelector<HTMLElement>(
        'a.room-link[aria-current="page"]',
      )!
      const other = document.querySelector<HTMLElement>(
        'a.room-link:not([aria-current])',
      )!
      const cs = getComputedStyle(sel)
      return {
        background: cs.backgroundColor,
        boxShadow: cs.boxShadow,
        color: cs.color,
        weight: getComputedStyle(sel.querySelector('.room-name')!).fontWeight,
        otherBackground: getComputedStyle(other).backgroundColor,
      }
    })

    expect(style.background).not.toBe(style.otherBackground)
    expect(style.boxShadow).toContain('inset') // the left accent bar
    expect(style.weight).toBe('600')
    expect(contrast(style.color, style.background)).toBeGreaterThan(7) // AAA
  })
}

test('arrow-key focus stays visible on the room links', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize(VIEWPORTS[2])
  await page.goto(ROOM_URL)
  // Control+K is subscribed in an effect. Pressing it the instant the
  // document loads misses the listener — Playwright hits that, a human
  // cannot. Live means first paint and effects have flushed, same as the
  // dedicated Ctrl-K coverage in `shortcuts.spec.ts`.
  await expectLive(page)

  await page.keyboard.press('Control+k')
  await expect.poll(() => active(page)).toContain('Filter by name')
  await page.keyboard.press('ArrowDown')

  const outline = await page.evaluate(() => {
    const el = document.activeElement as HTMLElement
    const cs = getComputedStyle(el)
    return { cls: el.className, width: cs.outlineWidth, style: cs.outlineStyle }
  })
  // `.room-list` clips overflow, so a default outline would be invisible.
  expect(outline.cls).toContain('room-link')
  expect(outline.style).toBe('solid')
  expect(parseFloat(outline.width)).toBeGreaterThanOrEqual(2)
})
