import type { CDPSession, Page } from '@playwright/test'

/**
 * Helpers for the timeline → room-list transition perf lane (ADR 0071). The app
 * already emits `axon:*` performance marks across the transition (`src/perf.ts`);
 * these turn a real headless-Chromium run into a phase breakdown we can read
 * back, under an emulated slow CPU that stands in for the reporter's iPhone 13.
 */

export type AxonMark = { name: string; t: number; detail: unknown }

/** Turn on `?perf=1`-gated marks for every load in this context. */
export async function enablePerf(page: Page): Promise<void> {
  await page.addInitScript(() => sessionStorage.setItem('axon.perf', '1'))
}

/**
 * Emulate a slower CPU via DevTools `setCPUThrottlingRate`: `rate` is a slowdown
 * multiplier (1 = native). Mid-range phones land roughly 4–6× slower than the
 * desktop Chromium this lane runs on — enough to surface main-thread work that a
 * 15 Pro Max hides.
 *
 * CDP is Chromium-only, so on WebKit (and at rate 1) this is a no-op returning
 * `null`: the WebKit lane runs at native speed, where the signal is the engine
 * itself, not a throttle. `unthrottle` tolerates the null.
 */
export async function throttleCpu(
  page: Page,
  rate: number,
): Promise<CDPSession | null> {
  const engine = page.context().browser()?.browserType().name()
  if (engine !== 'chromium' || rate === 1) {
    return null
  }
  const cdp = await page.context().newCDPSession(page)
  await cdp.send('Emulation.setCPUThrottlingRate', { rate })
  return cdp
}

/**
 * Undo `throttleCpu`: reset the rate and detach the per-cell CDP session so it
 * isn't left open across the sweep. Safe to call with the `null` a no-op
 * throttle returned.
 */
export async function unthrottle(cdp: CDPSession | null): Promise<void> {
  if (cdp !== null) {
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: 1 })
    await cdp.detach()
  }
}

/**
 * A weak mobile link, as DevTools models one: throughput in **bytes** per
 * second, latency in milliseconds added to every request.
 *
 * `slow-3g` is the profile that matters here — the complaint this lane exists
 * for is a room that takes tens of seconds to paint on a bad cell connection,
 * and the hypothesis is that the timeline page is queued behind the three
 * larger bodies `RoomPage` requests in the same breath. That only shows up
 * when the link cannot carry them all at once.
 */
export const NETWORK_PROFILES = {
  'slow-3g': { latency: 400, download: 50_000, upload: 20_000 },
  'fast-3g': { latency: 150, download: 180_000, upload: 84_000 },
  offline: { latency: 0, download: 0, upload: 0 },
} as const

export type NetworkProfile = keyof typeof NETWORK_PROFILES

/**
 * Emulate a slow network via DevTools `Network.emulateNetworkConditions`.
 *
 * Chromium-only, with the same no-op-returning-`null` shape `throttleCpu`
 * uses, and undone by the same `unthrottle`. Note this throttles the *whole
 * context*, the app bundle included — so a spec that wants to measure a room
 * open on a slow link should load the app first and throttle afterwards,
 * or it measures the bundle download instead.
 */
export async function throttleNetwork(
  page: Page,
  profile: NetworkProfile,
): Promise<CDPSession | null> {
  const engine = page.context().browser()?.browserType().name()
  if (engine !== 'chromium') {
    return null
  }
  const { latency, download, upload } = NETWORK_PROFILES[profile]
  const cdp = await page.context().newCDPSession(page)
  await cdp.send('Network.enable')
  await cdp.send('Network.emulateNetworkConditions', {
    offline: profile === 'offline',
    latency,
    downloadThroughput: download,
    uploadThroughput: upload,
  })
  return cdp
}

/**
 * Undo `throttleNetwork`. Separate from `unthrottle` because the two send
 * different CDP domains, and a leaked network throttle is far worse than a
 * leaked CPU one — every later spec in the shard would time out.
 */
export async function unthrottleNetwork(cdp: CDPSession | null): Promise<void> {
  if (cdp !== null) {
    await cdp.send('Network.emulateNetworkConditions', {
      offline: false,
      latency: 0,
      downloadThroughput: -1,
      uploadThroughput: -1,
    })
    await cdp.detach()
  }
}

/**
 * The detail of the last `boot:room-open` summary the page emitted, or `null`.
 *
 * Read from `performance` rather than the overlay, so a spec does not depend on
 * how many lines the overlay happens to keep.
 */
export async function roomOpenSummary(
  page: Page,
): Promise<Record<string, unknown> | null> {
  return page.evaluate(() => {
    const marks = performance
      .getEntriesByType('mark')
      .filter((mark) => mark.name === 'axon:boot:room-open')
    const last = marks[marks.length - 1] as PerformanceMark | undefined
    return (last?.detail as Record<string, unknown> | undefined) ?? null
  })
}

/** Every `boot:room-open:req` detail the page emitted, oldest first. */
export async function roomOpenRequests(
  page: Page,
): Promise<Record<string, unknown>[]> {
  return page.evaluate(() =>
    performance
      .getEntriesByType('mark')
      .filter((mark) => mark.name === 'axon:boot:room-open:req')
      .map(
        (mark) =>
          ((mark as PerformanceMark).detail as Record<string, unknown>) ?? {},
      ),
  )
}

/** Every `axon:*` mark currently on the timeline, name-stripped of the prefix. */
export async function readAxonMarks(page: Page): Promise<AxonMark[]> {
  return page.evaluate(() =>
    performance
      .getEntriesByType('mark')
      .filter((m) => m.name.startsWith('axon:'))
      .map((m) => ({
        name: m.name.slice('axon:'.length),
        t: m.startTime,
        detail: (m as PerformanceMark).detail,
      })),
  )
}

/** Wait for the two `requestAnimationFrame`s a `perfMarkFrames` pair spans. */
export async function settleFrames(page: Page): Promise<void> {
  await page.evaluate(
    () =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() =>
          requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
        )
      }),
  )
}

/**
 * Pair sequential start/end marks into durations. Renders here are sequential,
 * not nested, so a simple open/close walk is exact and tolerates the estimate →
 * measure → correct churn that produces several passes per transition.
 */
function pairDurations(
  marks: AxonMark[],
  startName: string,
  endName: string,
): number[] {
  const evs = marks
    .filter((m) => m.name === startName || m.name === endName)
    .sort((a, b) => a.t - b.t)
  const durs: number[] = []
  let open: number | null = null
  for (const m of evs) {
    if (m.name === startName) {
      open = m.t
    } else if (open !== null) {
      durs.push(m.t - open)
      open = null
    }
  }
  return durs
}

/**
 * `base:now` → `base:raf2` wall time (a `perfMarkFrames` frames-to-paint span).
 * The transition fires several render passes, so pair the *last* `:now` with the
 * `:raf2` that follows it — the final pass, the one that actually settles.
 */
function frameSpan(marks: AxonMark[], base: string): number | null {
  const nows = marks.filter((m) => m.name === `${base}:now`)
  if (nows.length === 0) {
    return null
  }
  const lastNow = nows[nows.length - 1].t
  const raf2 = marks
    .filter((m) => m.name === `${base}:raf2` && m.t >= lastNow)
    .sort((a, b) => a.t - b.t)[0]
  return raf2 ? raf2.t - lastNow : null
}

const sum = (xs: number[]): number => xs.reduce((a, b) => a + b, 0)

export type PhaseBreakdown = {
  /** Wall time from the start mark to the last room-list render pass. */
  total: number
  /** Time attributable to the room list itself (compute + measure + render). */
  listPhase: number
  /** How many times the list re-rendered during the transition (churn). */
  renderPasses: number
  visibleCompute: number
  measure: number
  postRenderFrames: number | null
  /** Rooms the list computed over, from the `visible-compute` detail. */
  rooms: number | null
}

/**
 * Break the transition down from marks emitted at or after `startName`. `total`
 * is the headline the hypothesis turns on: if it grows with timeline length
 * while `listPhase` stays flat, the cost is `RoomPage` teardown, not the list.
 */
export function phaseBreakdown(
  marks: AxonMark[],
  startName: string,
): PhaseBreakdown {
  const start = marks.find((m) => m.name === startName)
  const t0 = start ? start.t : 0
  const after = marks.filter((m) => m.t >= t0)

  const renders = after.filter((m) => m.name === 'room-list:render')
  const lastRender = renders.length > 0 ? renders[renders.length - 1].t : t0
  const postRenderFrames = frameSpan(after, 'room-list:post-render')

  const visibleCompute = sum(
    pairDurations(
      after,
      'room-list:visible-compute:start',
      'room-list:visible-compute:end',
    ),
  )
  const measure = sum(
    pairDurations(after, 'room-list:measure:start', 'room-list:measure:end'),
  )

  const firstCompute = after.find(
    (m) => m.name === 'room-list:visible-compute:start',
  )
  const rooms =
    firstCompute &&
    typeof (firstCompute.detail as { rooms?: number })?.rooms === 'number'
      ? (firstCompute.detail as { rooms: number }).rooms
      : null

  return {
    total: lastRender - t0,
    listPhase: visibleCompute + measure,
    renderPasses: renders.length,
    visibleCompute,
    measure,
    postRenderFrames,
    rooms,
  }
}

/** A one-line, fixed-width row for the matrix the spec prints and attaches. */
export function breakdownRow(label: string, b: PhaseBreakdown): string {
  const ms = (n: number | null) =>
    n === null ? '   —  ' : n.toFixed(1).padStart(6)
  return (
    `${label.padEnd(22)}` +
    ` total=${ms(b.total)}ms` +
    ` list=${ms(b.listPhase)}ms` +
    ` (compute=${ms(b.visibleCompute)} measure=${ms(b.measure)})` +
    ` passes=${String(b.renderPasses).padStart(2)}` +
    ` post-frames=${ms(b.postRenderFrames)}ms`
  )
}
