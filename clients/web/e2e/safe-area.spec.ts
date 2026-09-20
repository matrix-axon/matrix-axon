import { expect, test, type Page } from '@playwright/test'
import { expectLive, ROOM_URL, signIn } from './helpers'

/**
 * The safe-area form-factor matrix (ADR 0105).
 *
 * `index.html` sets `viewport-fit=cover`, so on a notched or home-indicator
 * device the app is laid out *over* the status bar, the Dynamic Island and the
 * home indicator, and `index.css` hands each strip back deliberately. Nothing
 * in this lane could ever see that: `env(safe-area-inset-*)` is `0px` in every
 * headless browser, which is exactly why "the test suite passing says nothing
 * here, checked on device instead" was the honest note on the change that
 * introduced it — and why the bug it introduced (#460) survived review.
 *
 * So the insets arrive from the outside. `index.css` reads all four through the
 * `--safe-*` custom properties, and this spec sets them as inline properties on
 * `:root`, which beats the stylesheet's own `:root` rule. What the matrix below
 * feeds in are **measured** numbers, not plausible ones: each row was read out
 * of the packaged Tauri shell running on that simulator, iOS/iPadOS 26.5,
 * 2026-09-20. See "Measuring the packaged shell" in AGENTS.md for how.
 *
 * What this proves and what it does not: it proves our layout arithmetic is
 * right for the insets a real device reports, on every form factor at once, in
 * CI, on every PR. It does not prove iOS still reports those insets — only a
 * device or a simulator can say that, and the numbers here are the record of
 * what it said.
 */

/** Both edges of a layout comparison are fractional; one rendered pixel is not a gap. */
const EDGE_TOLERANCE_PX = 1

/**
 * How much blank space may sit under the message entry box *beyond* the home
 * indicator it has to clear. The composer's own bottom padding is `0.6rem`
 * (9.6px) at two-pane widths and `0.15rem` (2.4px) below the breakpoint, so
 * this is that plus rounding room — and far below the 40px of double-counted
 * inset that #460 was.
 */
const COMPOSER_SLACK_PX = 12

type Insets = { top: number; right: number; bottom: number; left: number }

type FormFactor = {
  /** Simulator device name, as `xcrun simctl list devicetypes` spells it. */
  name: string
  orientation: 'portrait' | 'landscape'
  viewport: { width: number; height: number }
  insets: Insets
}

/**
 * Measured on the iOS 26.5 simulator, in the packaged shell, 2026-09-20.
 *
 * Worth reading the numbers rather than skimming them, because they are not
 * what a guess would produce:
 *
 * - The bottom inset is **not** one value. 34px on a phone in portrait, 20px on
 *   the same phone in landscape, 20px on every iPad, and 0 on an iPhone SE.
 *   A single constant would be wrong four ways.
 * - An iPad mini is **744pt wide in portrait**, which is below the 48rem
 *   (768px) two-pane breakpoint — so the smallest tablet lays out on the phone
 *   branch in portrait and the tablet branch in landscape. It is the one device
 *   that crosses the breakpoint by rotating, and so the one that catches a fix
 *   applied to only one side of it.
 * - A phone in **landscape** is 874pt wide and therefore on the tablet branch.
 *   #460 was reported on an iPad and reproduced on every phone in landscape.
 * - An iPhone SE has no *bottom* inset — no home indicator — but still reports
 *   20px at the top for the status bar. It is the control row, and the one that
 *   shows a change here is invisible on hardware without a notch.
 *
 * The iPad landscape rows carry the portrait bottom inset deliberately. The
 * only way to read a landscape simulator headlessly is a build that declares
 * no portrait orientation, and on an iPad that also makes the app ineligible
 * for multitasking, which by itself reports 25px instead of 20px — reproduced
 * exactly by setting `UIRequiresFullScreen` on a build that keeps all four
 * orientations. 20px is what the shipping configuration reports, and it is the
 * stricter of the two to assert against. See ADR 0105 § Consequences.
 */
const FORM_FACTORS: FormFactor[] = [
  {
    name: 'iPhone SE (3rd generation)',
    orientation: 'portrait',
    viewport: { width: 375, height: 667 },
    insets: { top: 20, right: 0, bottom: 0, left: 0 },
  },
  {
    name: 'iPhone SE (3rd generation)',
    orientation: 'landscape',
    viewport: { width: 667, height: 375 },
    insets: { top: 0, right: 0, bottom: 0, left: 0 },
  },
  {
    name: 'iPhone 17',
    orientation: 'portrait',
    viewport: { width: 402, height: 874 },
    insets: { top: 62, right: 0, bottom: 34, left: 0 },
  },
  {
    name: 'iPhone 17',
    orientation: 'landscape',
    viewport: { width: 874, height: 402 },
    insets: { top: 0, right: 62, bottom: 20, left: 62 },
  },
  {
    name: 'iPhone 17 Pro Max',
    orientation: 'portrait',
    viewport: { width: 440, height: 956 },
    insets: { top: 62, right: 0, bottom: 34, left: 0 },
  },
  {
    name: 'iPad mini (A17 Pro)',
    orientation: 'portrait',
    viewport: { width: 744, height: 1133 },
    insets: { top: 32, right: 0, bottom: 20, left: 0 },
  },
  {
    name: 'iPad mini (A17 Pro)',
    orientation: 'landscape',
    viewport: { width: 1133, height: 744 },
    insets: { top: 32, right: 0, bottom: 20, left: 0 },
  },
  {
    name: 'iPad Pro 11-inch (M4)',
    orientation: 'portrait',
    viewport: { width: 834, height: 1210 },
    insets: { top: 32, right: 0, bottom: 20, left: 0 },
  },
  {
    name: 'iPad Pro 13-inch (M4)',
    orientation: 'landscape',
    viewport: { width: 1376, height: 1032 },
    insets: { top: 32, right: 0, bottom: 20, left: 0 },
  },
]

/**
 * Give the page the safe areas a real device would report.
 *
 * Inline properties on the root element, not an injected stylesheet: inline
 * style beats any `:root` selector without needing `!important`, and it is the
 * same surface `app.tsx` already writes `--app-viewport-*` through, so nothing
 * new has to be true for this to work.
 */
async function applySafeAreas(page: Page, insets: Insets): Promise<void> {
  await page.addInitScript((measured: Insets) => {
    const apply = () => {
      const root = document.documentElement
      if (root === null) {
        return false
      }
      root.style.setProperty('--safe-top', `${measured.top}px`)
      root.style.setProperty('--safe-right', `${measured.right}px`)
      root.style.setProperty('--safe-bottom', `${measured.bottom}px`)
      root.style.setProperty('--safe-left', `${measured.left}px`)
      return true
    }
    // An init script runs before the document has an element to style, so the
    // first call is expected to do nothing and the listener is what lands it.
    // Registering the listener *first* matters: an earlier version called
    // `apply()` up front, threw on a null `documentElement`, and never reached
    // the `addEventListener` below — so every row silently ran with no insets
    // at all and every assertion measured the same zero.
    document.addEventListener('DOMContentLoaded', apply)
    apply()
  }, insets)
}

type RoomGeometry = {
  shellTop: number
  shellBottom: number
  shellLeft: number
  shellRight: number
  composerBottom: number
  composerLeft: number
  composerRight: number
  textareaBottom: number
  topbarContentTop: number
  documentScrollWidth: number
}

async function roomGeometry(page: Page): Promise<RoomGeometry> {
  return page.evaluate(() => {
    const box = (selector: string) =>
      document.querySelector<HTMLElement>(selector)!.getBoundingClientRect()
    const shell = box('.shell')
    const composer = box('.room-stream > .composer')
    const textarea = box('.room-stream .composer textarea')
    // The first thing the topbar actually draws, whatever the breakpoint calls
    // it: the brand lockup at two-pane widths, the room heading below it.
    const topbarContent = document.querySelector<HTMLElement>(
      '.topbar-brand-lockup, .topbar-room-heading',
    )!
    return {
      shellTop: shell.top,
      shellBottom: shell.bottom,
      shellLeft: shell.left,
      shellRight: shell.right,
      composerBottom: composer.bottom,
      composerLeft: composer.left,
      composerRight: composer.right,
      textareaBottom: textarea.bottom,
      topbarContentTop: topbarContent.getBoundingClientRect().top,
      documentScrollWidth: document.documentElement.scrollWidth,
    }
  })
}

for (const factor of FORM_FACTORS) {
  const label = `${factor.name} ${factor.orientation}`

  test(`room layout fits the display: ${label}`, async ({ page }) => {
    await signIn(page)
    await applySafeAreas(page, factor.insets)
    await page.setViewportSize(factor.viewport)
    await page.goto(ROOM_URL)
    await expectLive(page)
    await expect(page.getByRole('textbox', { name: /^Message/ })).toBeVisible()

    const geometry = await roomGeometry(page)
    const { bottom, top, left, right } = factor.insets

    // The shell fills the display. `viewport-fit=cover` is what makes this
    // true; without it iOS lays the page out inside the safe areas and fills
    // the rest with the webview's own backdrop.
    expect(geometry.shellTop).toBe(0)
    expect(geometry.shellBottom).toBeCloseTo(factor.viewport.height, 0)

    // The composer is the element against the bottom edge in a room, and is
    // therefore the one — the only one — that pays the bottom inset. #460 was
    // `main` paying it a second time *below* the composer, which both left a
    // dead band and stopped the composer reaching the edge it was written for.
    expect(geometry.shellBottom - geometry.composerBottom).toBeLessThanOrEqual(
      EDGE_TOLERANCE_PX,
    )

    // The entry box clears the home indicator, and clears it by the inset and
    // not much more. Both directions matter: too little and the indicator is
    // drawn across the text box (the phone half of #460), too much and the app
    // stops short of its own bottom edge (the tablet half).
    const belowTextarea = geometry.shellBottom - geometry.textareaBottom
    expect(belowTextarea).toBeGreaterThanOrEqual(bottom - EDGE_TOLERANCE_PX)
    expect(belowTextarea).toBeLessThanOrEqual(bottom + COMPOSER_SLACK_PX)

    // The status bar and the Dynamic Island sit above the topbar's content,
    // not on top of it.
    expect(geometry.topbarContentTop).toBeGreaterThanOrEqual(
      top - EDGE_TOLERANCE_PX,
    )

    // A phone in landscape has a notch on one side and rounded corners on
    // both: `.shell` hands those back, so nothing is drawn into them and the
    // page still does not scroll sideways.
    expect(geometry.composerLeft).toBeGreaterThanOrEqual(
      left - EDGE_TOLERANCE_PX,
    )
    expect(geometry.composerRight).toBeLessThanOrEqual(
      factor.viewport.width - right + EDGE_TOLERANCE_PX,
    )
    expect(geometry.documentScrollWidth).toBeLessThanOrEqual(
      factor.viewport.width,
    )
  })
}

/**
 * The other half of the ownership rule: a page that is *not* a room still ends
 * in whatever control came last, so `main` keeps the inset there. Zeroing it
 * for every route would put the last row of Settings under the home indicator
 * with no way to scroll it clear, which is the bug the inset was added for.
 */
test('a scrolling utility page keeps the bottom inset `main` owns', async ({
  page,
}) => {
  const insets = { top: 62, right: 0, bottom: 34, left: 0 }
  await signIn(page)
  await applySafeAreas(page, insets)
  await page.setViewportSize({ width: 402, height: 874 })
  await page.goto('/settings')
  await expect(page.getByRole('heading', { name: 'Settings' })).toBeVisible()

  const padding = await page.evaluate(() =>
    Number.parseFloat(
      getComputedStyle(document.querySelector('.shell main')!).paddingBottom,
    ),
  )
  expect(padding).toBeGreaterThanOrEqual(insets.bottom)
})

/**
 * The two panel columns, which the room cases above never open.
 *
 * Each is its own answer to the same question. The thread ends in a
 * `.composer`, so the panel reserves nothing and the composer owns the edge,
 * exactly as it does in the room. Room info ends in a member row and has no
 * composer, so the panel itself reserves the inset.
 *
 * Both are covered at three widths on purpose, because the panels change layout
 * twice: a fixed overlay drawer below 64rem — on a phone *and* on an iPad in
 * portrait, which is past the two-pane breakpoint but not past this one — and a
 * static third column above it. A rule fixed on one side of that and not the
 * other is precisely what this file exists to catch.
 */
const PANEL_FACTORS: FormFactor[] = [
  {
    name: 'iPhone 17',
    orientation: 'portrait',
    viewport: { width: 402, height: 874 },
    insets: { top: 62, right: 0, bottom: 34, left: 0 },
  },
  {
    name: 'iPad Pro 11-inch (M4)',
    orientation: 'portrait',
    viewport: { width: 834, height: 1210 },
    insets: { top: 32, right: 0, bottom: 20, left: 0 },
  },
  {
    name: 'iPad Pro 13-inch (M4)',
    orientation: 'landscape',
    viewport: { width: 1376, height: 1032 },
    insets: { top: 32, right: 0, bottom: 20, left: 0 },
  },
]

for (const factor of PANEL_FACTORS) {
  const label = `${factor.name} ${factor.orientation}`

  test(`the thread composer reaches the bottom edge: ${label}`, async ({
    page,
  }) => {
    await signIn(page)
    await applySafeAreas(page, factor.insets)
    await page.setViewportSize(factor.viewport)
    await page.goto(`${ROOM_URL}?thread=%24root`)
    await expect(page.locator('.thread-panel')).toBeVisible()
    await expect(
      page.locator('.thread-panel').getByRole('textbox', {
        name: 'Reply in thread',
      }),
    ).toBeVisible()

    const geometry = await page.evaluate(() => {
      const box = (selector: string) =>
        document.querySelector<HTMLElement>(selector)!.getBoundingClientRect()
      return {
        panelBottom: box('.thread-panel').bottom,
        composerBottom: box('.thread-panel > .composer').bottom,
        textareaBottom: box('.thread-panel .composer textarea').bottom,
      }
    })

    // The drawer is `position: fixed` to the display; the column is inside a
    // `main` with no bottom padding in a room. Either way the panel's own
    // bottom is the display's.
    expect(geometry.panelBottom).toBeCloseTo(factor.viewport.height, 0)
    expect(geometry.panelBottom - geometry.composerBottom).toBeLessThanOrEqual(
      EDGE_TOLERANCE_PX,
    )
    const belowTextarea = geometry.panelBottom - geometry.textareaBottom
    expect(belowTextarea).toBeGreaterThanOrEqual(
      factor.insets.bottom - EDGE_TOLERANCE_PX,
    )
    expect(belowTextarea).toBeLessThanOrEqual(
      factor.insets.bottom + COMPOSER_SLACK_PX,
    )
  })

  test(`room information reserves the home indicator: ${label}`, async ({
    page,
  }) => {
    await signIn(page)
    await applySafeAreas(page, factor.insets)
    await page.setViewportSize(factor.viewport)
    await page.goto(ROOM_URL)
    await expect(page.locator('.timeline')).toBeVisible()
    await page
      .getByRole('button', { name: 'Open room information' })
      .first()
      .click()
    await expect(
      page.getByRole('complementary', { name: 'Room information' }),
    ).toBeVisible()

    const geometry = await page.evaluate(() => {
      const panel = document.querySelector<HTMLElement>('.side-panel')!
      return {
        panelBottom: panel.getBoundingClientRect().bottom,
        // Where the panel's *content* stops — the scroller inside it can reach
        // this and no further, so it is the row that would sit under the home
        // indicator if the inset were missing.
        paddingBottom: Number.parseFloat(getComputedStyle(panel).paddingBottom),
      }
    })

    expect(geometry.panelBottom).toBeCloseTo(factor.viewport.height, 0)
    expect(geometry.paddingBottom).toBeGreaterThanOrEqual(
      factor.insets.bottom - EDGE_TOLERANCE_PX,
    )
  })
}
