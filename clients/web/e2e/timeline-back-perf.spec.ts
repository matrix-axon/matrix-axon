import { expect, test, type Page } from '@playwright/test'
import { ROOM_URL, signIn } from './helpers'
import {
  breakdownRow,
  enablePerf,
  phaseBreakdown,
  type PhaseBreakdown,
  readAxonMarks,
  settleFrames,
  throttleCpu,
  unthrottle,
} from './perf-helpers'

/**
 * Perf diagnostic for the reported slow "timeline → room list" switch on a phone
 * (ADR 0071). One user (iPhone 13) feels it; a 15 Pro Max does not.
 *
 * Two questions, two engines:
 *
 *  - Chromium (with CDP CPU throttling): does the cost scale with timeline
 *    length? It does — `RoomPage` renders a row per event with no windowing, and
 *    tearing that subtree down on the switch is O(events). That is the finding
 *    the `content-visibility` mitigation targets.
 *
 *  - WebKit (Apple's engine, native speed — CDP throttling is Chromium-only):
 *    a later report reproduced the slowness on an account with SIX rooms and no
 *    long timelines, which the scaling story cannot explain. That points at a
 *    *constant* cost, possibly engine-specific, invisible to a Chromium run. The
 *    WebKit lane measures the reported small config on the real engine so an
 *    engine-specific constant cost shows up as a higher baseline than Chromium's.
 *
 * Both engines always measure the REPORTED cell (≈6 rooms, short timeline, native
 * speed) so the two runs are directly comparable. Opt-in and machine-sensitive:
 * `pnpm test:e2e:perf` (`PERF=1`); skipped in the default e2e sweep.
 */

// The reported small account: 3 seed rooms + 3 filler ≈ 6, short timeline.
const REPORTED = { bulkRooms: 3, timeline: 50 }
// The scaling sweep (Chromium only): a power-user room count, growing timelines.
const SWEEP_ROOMS = 147 // + 3 seed = 150
const TIMELINE_LENGTHS = [50, 500, 2000]
const CPU_RATES = [1, 4, 6]
const ASSERTION_SAMPLES = 3
const MAX_LIST_SHARE_OF_TOTAL_INCREASE = 0.25
const RUN = !!process.env.PERF

type Cell = { bulkRooms: number; timeline: number; rate: number }
type ScalingEvidence = {
  shortTotal: number
  longTotal: number
  shortList: number
  longList: number
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b)
  const upperMiddle = Math.floor(sorted.length / 2)
  if (sorted.length % 2 === 1) {
    return sorted[upperMiddle]
  }
  return (sorted[upperMiddle - 1] + sorted[upperMiddle]) / 2
}

/** Post fixtures, load the room, throttle, do the switch, read the breakdown. */
async function measureCell(
  page: Page,
  request: import('@playwright/test').APIRequestContext,
  cell: Cell,
): Promise<PhaseBreakdown> {
  await request.post(`/__e2e/bulk-rooms?count=${cell.bulkRooms}`)
  await request.post(`/__e2e/bulk-timeline?count=${cell.timeline}`)

  // Load the open room at native speed; only the transition is throttled.
  await page.goto(ROOM_URL)
  await expect(page.locator('.event-list')).toBeVisible()
  await expect(page.getByText('Bulk message 0')).toBeAttached()
  await expect(page.locator('.topbar-rooms-button')).toBeVisible()

  const cdp = await throttleCpu(page, cell.rate)

  // Bracket the transition, then tap the phone "Rooms" affordance.
  await page.evaluate(() => {
    performance.clearMarks()
    performance.mark('axon:e2e:back-start')
  })
  await page.locator('.topbar-rooms-button').click()
  await expect(page.locator('.room-list a.room-link').first()).toBeVisible()
  await settleFrames(page)

  const breakdown = phaseBreakdown(await readAxonMarks(page), 'e2e:back-start')

  await unthrottle(cdp)
  await request.post('/__e2e/bulk-rooms?count=0')
  await request.post('/__e2e/bulk-timeline?count=0')
  return breakdown
}

test.describe('timeline → room-list transition cost', () => {
  test.skip(!RUN, 'perf lane — set PERF=1 (pnpm test:e2e:perf) to run')
  test.setTimeout(360_000)

  test.beforeEach(async ({ page }) => {
    await enablePerf(page)
    await signIn(page)
    await page.setViewportSize({ width: 390, height: 844 }) // iPhone-class, single pane
  })

  test('transition cost by engine, data size, and CPU', async ({
    page,
    request,
  }, testInfo) => {
    const engine = testInfo.project.name // 'chromium' | 'webkit'
    const lines: string[] = []
    let scalingEvidence: ScalingEvidence | null = null

    // A discarded warmup transition: the first navigation pays one-time costs
    // (first paint, font load, JIT) that would otherwise land entirely on the
    // REPORTED cell and inflate it — badly on WebKit, which has a slow cold
    // start. Measuring after warmup isolates the steady-state transition cost.
    await measureCell(page, request, { ...REPORTED, rate: 1 })

    // Both engines: the REPORTED small config at native speed. This is the cell
    // the six-room report reproduces on, and the one that makes the two engine
    // runs directly comparable.
    const reported = await measureCell(page, request, { ...REPORTED, rate: 1 })
    lines.push(breakdownRow('REPORTED (~6 rooms)', reported))

    if (engine === 'chromium') {
      // Chromium can throttle: sweep timeline length × CPU rate for the scaling
      // story (and as a regression guard for the content-visibility fix).
      const slow = CPU_RATES[CPU_RATES.length - 1]
      const short = TIMELINE_LENGTHS[0]
      const long = TIMELINE_LENGTHS[TIMELINE_LENGTHS.length - 1]
      const samplesByCell = new Map<string, PhaseBreakdown[]>()
      for (const rate of CPU_RATES) {
        for (const timeline of TIMELINE_LENGTHS) {
          const key = `${rate}:${timeline}`
          const assertionEndpoint =
            rate === slow && (timeline === short || timeline === long)
          const sampleCount = assertionEndpoint ? ASSERTION_SAMPLES : 1
          const samples: PhaseBreakdown[] = []
          for (let sample = 1; sample <= sampleCount; sample += 1) {
            const b = await measureCell(page, request, {
              bulkRooms: SWEEP_ROOMS,
              timeline,
              rate,
            })
            const sampleLabel =
              sampleCount > 1 ? ` · sample ${sample}/${sampleCount}` : ''
            lines.push(
              breakdownRow(
                `cpu ${rate}× · timeline ${timeline}${sampleLabel}`,
                b,
              ),
            )
            samples.push(b)
          }
          samplesByCell.set(key, samples)
        }
      }

      const shortSamples = samplesByCell.get(`${slow}:${short}`)!
      const longSamples = samplesByCell.get(`${slow}:${long}`)!
      scalingEvidence = {
        shortTotal: median(shortSamples.map((sample) => sample.total)),
        longTotal: median(longSamples.map((sample) => sample.total)),
        shortList: median(shortSamples.map((sample) => sample.listPhase)),
        longList: median(longSamples.map((sample) => sample.listPhase)),
      }
      lines.push(
        `slow-CPU endpoint medians (${ASSERTION_SAMPLES} samples)` +
          ` total=${scalingEvidence.shortTotal.toFixed(1)}→${scalingEvidence.longTotal.toFixed(1)}ms` +
          ` list=${scalingEvidence.shortList.toFixed(1)}→${scalingEvidence.longList.toFixed(1)}ms`,
      )
    } else {
      // WebKit: no CDP throttle, so run the same timeline sweep at native speed.
      // The comparison that matters is WebKit-native vs Chromium-native at the
      // REPORTED cell above; the sweep here just shows WebKit's own scaling.
      for (const timeline of TIMELINE_LENGTHS) {
        const b = await measureCell(page, request, {
          bulkRooms: SWEEP_ROOMS,
          timeline,
          rate: 1,
        })
        lines.push(breakdownRow(`native · timeline ${timeline}`, b))
      }
    }

    const table = [`engine=${engine}`, ...lines].join('\n')
    console.log('\n' + table + '\n')
    await testInfo.attach(`transition-phase-matrix.${engine}.txt`, {
      body: table,
      contentType: 'text/plain',
    })

    if (scalingEvidence !== null) {
      // Directional (not absolute-ms) checks: at the slowest CPU a 40× longer
      // timeline costs materially more end to end...
      expect(scalingEvidence.longTotal).toBeGreaterThan(
        scalingEvidence.shortTotal * 1.5,
      )
      // ...and room-list work accounts for only a minority of that absolute
      // increase, leaving RoomPage teardown as the cost that scaled. Comparing
      // deltas avoids unstable ratios between near-zero room-list measurements.
      const totalIncrease =
        scalingEvidence.longTotal - scalingEvidence.shortTotal
      const listIncrease = scalingEvidence.longList - scalingEvidence.shortList
      expect(listIncrease).toBeLessThan(
        totalIncrease * MAX_LIST_SHARE_OF_TOTAL_INCREASE,
      )
    }

    // A sanity floor on every engine: the transition produced a real measurement.
    expect(reported.total).toBeGreaterThan(0)
  })
})
