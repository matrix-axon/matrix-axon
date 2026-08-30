import { readFileSync } from 'node:fs'
import { expect, type Locator, type Page } from '@playwright/test'

/**
 * The web demo driver (ADR 0086 phase 3).
 *
 * Everything a scene needs that is not the scene itself: reaching the seeded
 * world by corpus name, staging a signed-in page whose chrome is in a known
 * state, drawing a pointer the video can see, and pacing.
 *
 * Two rules the TUI pilot learned the hard way (ADR 0086) apply here verbatim,
 * and Playwright's auto-waiting only removes the first half of the first one:
 *
 * - **A step that changes state needs an assertion only the new state
 *   satisfies.** Prefer state-unique chrome over content that is on screen
 *   either way, and assert on what *left* (`toBeHidden`, `toHaveCount(0)`) to
 *   prove a narrowing step narrowed. An assertion the *previous* frame already
 *   satisfies lets the script run ahead of the client, and the next input lands
 *   somewhere unintended — on video.
 * - **A scene that mutates scripts its undo**, so it leaves the world as it
 *   found it. Otherwise its own second run is satisfied by what its first run
 *   left behind and the assertion quietly stops meaning anything.
 */

// ---------------------------------------------------------------------------
// The seeded world
// ---------------------------------------------------------------------------

interface DemoRoom {
  room_id: string
  name: string | null
  direct: boolean
  members: string[]
}

interface DemoPersona {
  user_id: string
  display_name: string
}

interface DemoSpace {
  room_id: string
  name: string
  children: string[]
}

/**
 * Only the parts of the manifest this driver actually reads.
 *
 * Not a mirror of the Rust `DemoManifest`: a mirror would have to be kept in
 * step by hand, and nothing here would fail if it drifted — the manifest is
 * plain JSON read at runtime, so an unused declared field is a claim TypeScript
 * never checks. (`seeded_at` is in the real manifest and absent here for
 * exactly that reason: no web scene names a date.) Add a field when a scene
 * needs it; `demo.events` and `demo.media` map corpus ids to Matrix ids and
 * `mxc://` URIs when one does.
 */
interface Manifest {
  axon_base_url: string
  axon_bearer_token: string
  demo?: {
    name: string
    personas: Record<string, DemoPersona>
    spaces: Record<string, DemoSpace>
    rooms: Record<string, DemoRoom>
  }
}

let cached: Manifest | null = null

/** The manifest of the `--corpus` stack this recording runs against. */
export function manifest(): Manifest {
  if (cached === null) {
    const path = process.env.DEMO_MANIFEST
    if (path === undefined || path === '') {
      throw new Error(
        'DEMO_MANIFEST is required; see playwright.demo.config.ts',
      )
    }
    cached = JSON.parse(readFileSync(path, 'utf8')) as Manifest
  }
  return cached
}

function demo(): NonNullable<Manifest['demo']> {
  const world = manifest().demo
  if (world === undefined) {
    throw new Error(
      'the stack was brought up without --corpus, so there is no demo world',
    )
  }
  return world
}

/**
 * Address the world by **corpus name**, never by Matrix id: room and event ids
 * are minted per run. `demo.toml` is the stable vocabulary, and the manifest is
 * the per-run mapping onto it.
 */
export function roomId(name: string): string {
  const room = demo().rooms[name]
  if (room === undefined) {
    throw new Error(
      `no corpus room ${name}; have: ${Object.keys(demo().rooms).join(', ')}`,
    )
  }
  return room.room_id
}

/** A persona's rendered display name — what a scene should assert against. */
export function personaName(id: string): string {
  const persona = demo().personas[id]
  if (persona === undefined) {
    throw new Error(
      `no corpus persona ${id}; have: ${Object.keys(demo().personas).join(', ')}`,
    )
  }
  return persona.display_name
}

/** A space's rendered name, for the picker's accessible labels. */
export function spaceName(id: string): string {
  const space = demo().spaces[id]
  if (space === undefined) {
    throw new Error(
      `no corpus space ${id}; have: ${Object.keys(demo().spaces).join(', ')}`,
    )
  }
  return space.name
}

let accountIdCache: string | null = null

/**
 * The axon account id the corpus viewer is logged in as.
 *
 * Room URLs are account-scoped (`/:accountId/rooms/:roomId`), and the id is
 * minted when the stack logs axon into the fixture account, so it cannot come
 * from the corpus. Node reaches axon directly here — no browser, no CORS.
 */
export async function accountId(): Promise<string> {
  if (accountIdCache !== null) {
    return accountIdCache
  }
  const { axon_base_url: base, axon_bearer_token: token } = manifest()
  const response = await fetch(`${base}/v1/accounts`, {
    headers: { authorization: `Bearer ${token}` },
  })
  if (!response.ok) {
    throw new Error(`GET /v1/accounts failed with ${response.status}`)
  }
  const body = (await response.json()) as { data?: { account_id: string }[] }
  const first = body.data?.[0]
  if (first === undefined) {
    throw new Error('axon has no accounts; did the stack finish logging in?')
  }
  accountIdCache = first.account_id
  return accountIdCache
}

/** The in-app URL of a corpus room. */
export async function roomUrl(name: string): Promise<string> {
  return `/${await accountId()}/rooms/${encodeURIComponent(roomId(name))}`
}

/**
 * Redact anything the corpus viewer previously left in `room` whose body
 * contains `needle`, over the API and before the browser opens.
 *
 * Mutating scenes script their own undo, but only a scene that *finishes*
 * gets there — an authoring run that fails halfway leaves its message behind,
 * and the next run then finds two copies and a strict-mode violation. (That is
 * not hypothetical: it is how this function came to exist.) Cleaning up here
 * rather than in the scene keeps the debris out of the recording, since a
 * scene's first frames would otherwise be it tidying up after itself.
 */
export async function resetRoom(room: string, needle: string): Promise<number> {
  const { axon_base_url: base, axon_bearer_token: token } = manifest()
  const account = await accountId()
  const encoded = encodeURIComponent(roomId(room))
  const headers = { authorization: `Bearer ${token}` }

  // One page, deliberately: the timeline reads newest-first and a cursorless
  // request is the newest page, while debris is by definition the newest thing
  // in the room — a scene sends at wall-clock now, and every corpus message is
  // backdated. So leftovers cannot fall off the end of this page unless a
  // single failed run left more than 100 of them behind, which would mean a
  // scene sending in a loop. Growing `demo.toml` does not erode it; the
  // guarantee is about ordering, not about corpus size.
  const page = await fetch(
    `${base}/v1/accounts/${account}/rooms/${encoded}/timeline?limit=100`,
    { headers },
  )
  if (!page.ok) {
    throw new Error(`GET timeline failed with ${page.status}`)
  }
  const body = (await page.json()) as {
    data?: {
      events?: { event_id: string; body?: string | null; redacted?: boolean }[]
    }
  }
  const stale = (body.data?.events ?? []).filter(
    (event) => event.redacted !== true && (event.body ?? '').includes(needle),
  )
  for (const event of stale) {
    const url = `${base}/v1/accounts/${account}/rooms/${encoded}/events/${encodeURIComponent(event.event_id)}`
    const response = await fetch(url, { method: 'DELETE', headers })
    if (!response.ok) {
      throw new Error(`redact ${event.event_id} failed: ${response.status}`)
    }
  }
  if (stale.length > 0) {
    // A redaction leaves a tombstone row behind forever, and nothing can
    // remove it — so this is not a clean slate, and the take will show
    // "message deleted" where a fresh world shows nothing. Fine while
    // authoring, wrong for the recording itself.
    console.warn(
      `demo: cleared ${stale.length} leftover message(s) from #${room}. ` +
        'Their tombstones remain: re-seed the stack before a real take.',
    )
  }
  return stale.length
}

// ---------------------------------------------------------------------------
// Pacing
// ---------------------------------------------------------------------------

/** Long enough to read a line. */
export const BEAT = 1200
/** Long enough to take in a whole screen. */
export const LONG = 2600

/**
 * Hold the frame so a viewer can read it — and *only* that. A dwell is never
 * how a scene waits for the client: every wait is an assertion, so the script
 * cannot desync on a slow machine. Scaled by `DEMO_PACE` (0 strips the dwells,
 * which makes a dry run fast).
 */
export async function dwell(page: Page, ms: number): Promise<void> {
  const factor = Number(process.env.DEMO_PACE ?? '1')
  const scaled = Math.round(ms * (Number.isFinite(factor) ? factor : 1))
  if (scaled > 0) {
    await page.waitForTimeout(scaled)
  }
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/**
 * Settings the recording depends on, seeded before any app script runs.
 *
 * `spacesPaneAutoHide` is the load-bearing one (ADR 0086): it defaults to
 * `true`, which hides the spaces rail as soon as there is no meaningful choice
 * — mid-scene, on video. The rest simply pin defaults a developer's own
 * browser profile has no way to influence, since this is a fresh context every
 * run; they are written down so a future default change cannot silently
 * restyle every recording.
 */
const DEMO_SETTINGS = {
  version: 1,
  // Pinned, not `system`. The shipped default follows `prefers-color-scheme`,
  // so on the default the recording's appearance would depend on the machine
  // it was made on — two takes of the same scene could disagree.
  theme: 'light',
  spacesPaneAutoHide: false,
  spacesPaneCollapsed: false,
  sidebarCollapsed: false,
  roomSort: 'recent',
  roomFilter: 'all',
  previewRoom: true,
  // Hidden, not `important`. `/createRoom` does not honour a backdated `ts`,
  // so the newest event in *every* corpus room is the viewer's own join — at
  // the default setting each room's timeline ends on "Alex Marx joined the
  // room", which reads as a broken client rather than as a seeded world.
  stateEvents: 'hidden',
  perfMarks: false,
  developerMode: false,
}

/**
 * A signed-in page at `url`, chrome in a known state, pointer overlay armed.
 *
 * The token is the stack's own disposable bearer token. It is seeded straight
 * into `localStorage` rather than typed into the paste form because the login
 * screen is not what any of these scenes are about — and a token on screen,
 * even a throwaway one, is a bad habit to record.
 */
export async function stage(page: Page, url: string): Promise<void> {
  const { axon_bearer_token: token } = manifest()
  await page.addInitScript(
    ({ token, settings }) => {
      localStorage.setItem('axon.token', token)
      localStorage.setItem('axon.settings', JSON.stringify(settings))
    },
    { token, settings: DEMO_SETTINGS },
  )
  await installPointer(page)
  await page.goto(url)
  // The socket is the honest "the app is ready" signal, and it is the real
  // one here — a live axon, not a mock. Located by class rather than by
  // `getByRole('status')`, which also matches the lightbox and search
  // overlays' own live regions once a scene has opened one.
  await expect(page.locator('.conn')).toHaveText('Live', { timeout: 60_000 })
}

// ---------------------------------------------------------------------------
// A pointer the video can see
// ---------------------------------------------------------------------------

/**
 * Playwright renders no cursor into video, so without this the UI appears to
 * operate itself (ADR 0086). Drawn as a DOM overlay in a `pointer-events: none`
 * layer.
 *
 * Two modes, chosen by the recording's own pointer media query rather than by a
 * flag, so a scene file cannot forget to set it:
 *
 * - **Fine pointer** (the desktop take): an arrow that follows `pointermove`
 *   and flashes a small ring on `pointerdown`.
 * - **Coarse pointer** (the mobile take): no arrow at all, only a touch ripple
 *   on `pointerdown`. Gating the arrow on `event.pointerType === 'mouse'` is
 *   *not* enough here — under Playwright's mobile emulation a `.tap()` emits a
 *   compatibility `pointermove` that still reports `pointerType: 'mouse'`, so
 *   the only reliable fix is to never create the arrow on a touch device.
 *
 * Registered as an init script so it survives a reload; the app is an SPA, so
 * in-app navigation never tears it down.
 */
async function installPointer(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const draw = () => {
      const coarse = matchMedia('(pointer: coarse)').matches

      const layer = document.createElement('div')
      layer.id = 'demo-pointer-layer'
      layer.setAttribute('aria-hidden', 'true')
      layer.style.cssText =
        'position:fixed;inset:0;pointer-events:none;z-index:2147483647'
      document.body.append(layer)

      let cursor: HTMLDivElement | null = null
      if (!coarse) {
        cursor = document.createElement('div')
        cursor.style.cssText =
          'position:absolute;width:22px;height:22px;margin:-2px 0 0 -2px;' +
          'opacity:0;transition:opacity 120ms linear;' +
          // A plain arrow, drawn rather than imported: an inline SVG data URI
          // keeps this self-contained and immune to the app's own asset paths.
          "background:no-repeat center/contain url('data:image/svg+xml;utf8," +
          encodeURIComponent(
            '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">' +
              '<path d="M4 2 L4 20 L9 15.5 L12.2 22 L15.6 20.4 L12.4 14 L19 14 Z" ' +
              'fill="#fff" stroke="#111" stroke-width="1.4" stroke-linejoin="round"/>' +
              '</svg>',
          ) +
          "')"
        layer.append(cursor)
      }

      const pulse = (x: number, y: number, touch: boolean) => {
        const ring = document.createElement('div')
        const size = touch ? 60 : 34
        ring.style.cssText =
          `position:absolute;left:${x}px;top:${y}px;width:${size}px;` +
          `height:${size}px;margin:${-size / 2}px 0 0 ${-size / 2}px;` +
          'border-radius:50%;box-sizing:border-box;' +
          // Dark ink plus a white halo, so the tap reads on a light or a dark
          // background alike — the recording pins the light theme, where the
          // old all-white ring was nearly invisible.
          'border:3px solid rgba(20,20,22,.6);' +
          'background:rgba(20,20,22,.12);' +
          'box-shadow:0 0 0 2px rgba(255,255,255,.85);' +
          'transform:scale(.3);opacity:1'
        layer.append(ring)
        ring.animate(
          [
            { transform: 'scale(.3)', opacity: 1 },
            { transform: 'scale(1)', opacity: 0 },
          ],
          { duration: touch ? 550 : 420, easing: 'ease-out' },
        ).onfinish = () => ring.remove()
      }

      if (cursor !== null) {
        const arrow = cursor
        addEventListener(
          'pointermove',
          (event) => {
            if (event.pointerType !== 'mouse') {
              return
            }
            arrow.style.opacity = '1'
            arrow.style.left = `${event.clientX}px`
            arrow.style.top = `${event.clientY}px`
          },
          { capture: true, passive: true },
        )
      }
      addEventListener(
        'pointerdown',
        (event) => {
          const touch = coarse || event.pointerType !== 'mouse'
          if (cursor !== null && touch) {
            cursor.style.opacity = '0'
          }
          pulse(event.clientX, event.clientY, touch)
        },
        { capture: true, passive: true },
      )
    }

    if (document.body === null) {
      addEventListener('DOMContentLoaded', draw, { once: true })
    } else {
      draw()
    }
  })
}

/**
 * Move to `target` and click it, with the mouse actually travelling there.
 *
 * `locator.click()` teleports the pointer to the element, which reads as a
 * glitch once there is a cursor on screen. The steps are what make the overlay
 * trace a path; the hit-testing and actionability checks are Playwright's own,
 * because the click still goes through the locator.
 */
export async function point(page: Page, target: Locator): Promise<void> {
  await target.scrollIntoViewIfNeeded()
  const box = await target.boundingBox()
  if (box === null) {
    throw new Error('cannot point at an element with no box')
  }
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, {
    steps: 24,
  })
  await target.click()
}

/**
 * Tap `target` — the mobile counterpart of {@link point}.
 *
 * A real touch event, not a click: the room list navigates on `pointerup` with
 * `pointerType: 'touch'` precisely so that touch *scrolling* over the list
 * never opens a room, and a synthesized click would not exercise that path.
 * The overlay's ripple is drawn from the same `pointerdown`.
 */
export async function tap(page: Page, target: Locator): Promise<void> {
  await target.scrollIntoViewIfNeeded()
  await target.tap()
}

/** `point`, but hovering only — for scenes that reveal chrome on hover. */
export async function hover(page: Page, target: Locator): Promise<void> {
  await target.scrollIntoViewIfNeeded()
  const box = await target.boundingBox()
  if (box === null) {
    throw new Error('cannot hover an element with no box')
  }
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, {
    steps: 24,
  })
  await target.hover()
}

/**
 * Type into `target` at a human rate.
 *
 * `fill()` sets the value in one frame, which on video looks like the text
 * teleporting in — and, for the search overlay, skips the incremental states
 * that are the interesting part.
 */
export async function typeInto(
  target: Locator,
  text: string,
  delay = 55,
): Promise<void> {
  await target.click()
  await target.pressSequentially(text, { delay })
}

/**
 * {@link typeInto}, but focusing the field with a touch tap — the mobile
 * counterpart, paired the way {@link point} and {@link tap} are.
 *
 * `typeInto` opens with `target.click()`, which drives `page.mouse` even under
 * a device descriptor (Playwright only emits touch through `.tap()`). On a
 * phone recording that wakes the pointer overlay's mouse branch and leaves an
 * arrow cursor parked on screen for the rest of the take — the overlay only
 * hides the arrow again on a *touch* `pointerdown`. Tapping to focus keeps the
 * take touch-only.
 */
export async function tapType(
  target: Locator,
  text: string,
  delay = 55,
): Promise<void> {
  await target.tap()
  await target.pressSequentially(text, { delay })
}
