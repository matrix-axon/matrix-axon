import {
  cleanup,
  fireEvent,
  render,
  waitFor,
  type RenderResult,
} from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider } from 'preact-iso'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import { createMembersStore } from '../stores/members'
import type { RoomDto } from '../stores/room-list'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomInfoPanel } from './RoomInfoPanel'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const FIRST = '!first:hs'
const SECOND = '!second:hs'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.unstubAllGlobals()
})
afterAll(() => server.close())

const room = (
  roomId: string,
  name: string,
  extra: Partial<RoomDto> = {},
): RoomDto =>
  ({
    account_id: ACCOUNT,
    account_user_id: '@me:example.org',
    room_id: roomId,
    name,
    last_activity_ts: 0,
    notification_count: 0,
    highlight_count: 0,
    ...extra,
  }) as unknown as RoomDto

/**
 * `/info` answers per room; every other read is empty. `failSecond` makes the
 * second room's reads fail, which is what a room switch while offline looks
 * like.
 */
function handlers(
  options: {
    failSecond?: boolean
    myLevel?: number
    /** Override the whole `users` map — `{}` is the room-v12 creator shape. */
    users?: Record<string, number>
  } = {},
) {
  const base = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  const infoFor = (roomId: string) =>
    decodeURIComponent(roomId) === FIRST
      ? { encryption_algorithm: 'm.megolm.v1.aes-sha2', join_rule: 'invite' }
      : { encryption_algorithm: null, join_rule: 'public' }
  return [
    http.get(`${base}/info`, ({ params }) => {
      const roomId = String(params.roomId)
      if (options.failSecond === true && decodeURIComponent(roomId) !== FIRST) {
        return HttpResponse.json(
          { error: { code: 'unavailable', message: 'offline' } },
          { status: 503 },
        )
      }
      return HttpResponse.json({ data: infoFor(roomId) })
    }),
    http.get(`${base}/pinned`, () => HttpResponse.json({ data: [] })),
    http.get(`${base}/space/children`, () => HttpResponse.json({ data: [] })),
    http.get(`${base}/space/parents`, () => HttpResponse.json({ data: [] })),
    http.get(`${base}/upgrade`, () =>
      HttpResponse.json({
        data: { upgraded_from: null, tombstoned_to: null },
      }),
    ),
    http.get(`${base}/members`, () => HttpResponse.json({ data: [] })),
    // The avatar viewer fetches the real bytes through the media proxy.
    http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:id`, () =>
      HttpResponse.arrayBuffer(new ArrayBuffer(8), {
        headers: { 'content-type': 'image/png' },
      }),
    ),
    http.get(`${base}/power_levels`, () =>
      HttpResponse.json({
        data: {
          ...powerLevels(options.myLevel ?? 100),
          ...(options.users === undefined ? {} : { users: options.users }),
        },
      }),
    ),
  ]
}

/** Resolved levels as the server returns them: `state_default` 50 gates edits. */
function powerLevels(myLevel: number) {
  return {
    ban: 50,
    invite: 0,
    kick: 50,
    redact: 50,
    events_default: 0,
    state_default: 50,
    users_default: 0,
    users: { '@me:example.org': myLevel },
  }
}

function renderPanel(roomId: string, extra: Partial<RoomDto> = {}) {
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, roomId)
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={roomId}
          room={room(roomId, roomId === FIRST ? 'First' : 'Second', extra)}
          roomTitles={new Map()}
          members={members}
          onClose={() => {}}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  const showRoom = (next: string) =>
    view.rerender(
      <ServicesContext.Provider value={services}>
        <LocationProvider>
          <RoomInfoPanel
            accountId={ACCOUNT}
            roomId={next}
            room={room(next, next === FIRST ? 'First' : 'Second')}
            roomTitles={new Map()}
            members={members}
            onClose={() => {}}
          />
        </LocationProvider>
      </ServicesContext.Provider>,
    )
  return { ...view, showRoom }
}

const detail = (label: string): string => {
  const terms = [...document.querySelectorAll('.detail-list dt')]
  const term = terms.find((node) => node.textContent === label)
  return term?.nextElementSibling?.textContent ?? ''
}

it('copies a populated detail on click and skips placeholders', async () => {
  const writeText = vi.fn().mockResolvedValue(undefined)
  vi.stubGlobal('navigator', { clipboard: { writeText } })
  server.use(...handlers())
  const { getByRole, findByRole, queryByRole } = renderPanel(FIRST)

  fireEvent.click(getByRole('button', { name: 'Copy Room ID' }))
  await waitFor(() => expect(writeText).toHaveBeenCalledWith(FIRST))
  expect((await findByRole('status')).textContent).toBe('Copied')

  fireEvent.click(getByRole('button', { name: 'Copy Name' }))
  await waitFor(() => expect(writeText).toHaveBeenCalledWith('First'))

  expect(queryByRole('button', { name: 'Copy Topic' })).toBeNull()
  expect(queryByRole('button', { name: 'Copy Full alias list' })).toBeNull()
})

it('shows a colored letter in the identity header when the room has no avatar', () => {
  server.use(...handlers())
  const { container } = renderPanel(FIRST)
  const identity = container.querySelector('.room-info-identity')
  const avatar = identity?.querySelector<HTMLElement>('.room-avatar')
  expect(identity?.querySelector('.room-info-identity-name')?.textContent).toBe(
    'First',
  )
  expect(identity?.querySelector('.room-info-identity-topic')).toBeNull()
  expect(avatar?.textContent).toBe('F')
  expect(avatar?.querySelector('img')).toBeNull()
  expect(avatar?.className).toMatch(/\broom-avatar-color-\d\b/)
})

it('shows the room avatar in the identity header', async () => {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/${ACCOUNT}/hs/avatar`,
      () =>
        new HttpResponse('avatar-bytes', {
          headers: { 'content-type': 'image/png' },
        }),
    ),
    ...handlers(),
  )
  const { container } = renderPanel(FIRST, { avatar_url: 'mxc://hs/avatar' })
  await waitFor(() => {
    const img = container.querySelector<HTMLImageElement>(
      '.room-info-identity .room-avatar img',
    )
    expect(img?.src).toMatch(/^blob:/)
  })
  expect(detail('Avatar')).toBe('mxc://hs/avatar')
})

it('shows the topic under the identity name when the room has one', () => {
  server.use(...handlers())
  const { container } = renderPanel(FIRST, { topic: 'Planning the next hike' })
  expect(
    container.querySelector('.room-info-identity-topic')?.textContent,
  ).toBe('Planning the next hike')
})

it('never shows the previous room’s state after a room switch', async () => {
  server.use(...handlers())
  const { showRoom } = renderPanel(FIRST)
  await waitFor(() => expect(detail('Encryption')).toBe('m.megolm.v1.aes-sha2'))

  // The panel is not remounted across room navigation, so a stale slice would
  // survive here — and encryption is exactly the field that must not lie.
  showRoom(SECOND)
  expect(detail('Encryption')).toBe('Loading…')
  await waitFor(() => expect(detail('Encryption')).toBe('Unencrypted'))
  expect(detail('Access')).toBe('public')
})

it('reports a failed room-state read instead of pending forever', async () => {
  server.use(...handlers({ failSecond: true }))
  const { showRoom, findByText } = renderPanel(FIRST)
  await waitFor(() => expect(detail('Encryption')).toBe('m.megolm.v1.aes-sha2'))

  showRoom(SECOND)
  await waitFor(() => expect(detail('Encryption')).toBe('Unavailable'))
  expect(await findByText('Could not load this section.')).toBeTruthy()
})

it('confirms before joining a relationship link, and reports a failed join', async () => {
  const base = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  let joins = 0
  server.use(
    http.get(`${base}/space/children`, () =>
      HttpResponse.json({
        data: [{ room_id: '!child:hs', name: 'General', via: ['hs'] }],
      }),
    ),
    http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
      joins += 1
      return HttpResponse.json(
        { error: { code: 'forbidden', message: 'no invite' } },
        { status: 403 },
      )
    }),
    ...handlers(),
  )
  const { findByRole, getByRole, findByText } = renderPanel(FIRST)

  // The label reads like navigation, but the room is not joined: opening it is
  // a membership change and must be confirmed.
  ;(await findByRole('button', { name: 'Child: General' })).click()
  expect(await findByText('Join this room?')).toBeTruthy()
  getByRole('button', { name: 'Cancel' }).click()
  expect(joins).toBe(0)
  ;(await findByRole('button', { name: 'Child: General' })).click()
  ;(await findByRole('button', { name: 'Join and open' })).click()

  await waitFor(() => expect(joins).toBe(1))
  expect(await findByText(/Could not join Child: General/)).toBeTruthy()
})

it('surfaces relationship errors rather than a permanent loading line', async () => {
  const base = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  server.use(
    // Every relationship read fails, as when the server is unreachable. These
    // come first: msw resolves with the first matching handler.
    http.get(`${base}/space/children`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
    http.get(`${base}/space/parents`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
    http.get(`${base}/upgrade`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
    ...handlers(),
  )
  const { queryAllByText, queryByText } = renderPanel(FIRST)

  // One per failed read: children, parents and upgrade.
  await waitFor(() =>
    expect(queryAllByText('Could not load this section.').length).toBe(3),
  )
  expect(queryByText('Loading room relationships…')).toBeNull()
})

/**
 * jsdom never loads resources, so a real `Image` fires neither `load` nor
 * `error` and the decode check would hang forever. Stub it to settle the way
 * a browser would for these bytes. The genuine decode is covered by the
 * Playwright lane, where a real engine reads a real corrupt file.
 */
function stubImageDecoding(readable: boolean): void {
  vi.stubGlobal(
    'Image',
    class {
      naturalWidth = readable ? 8 : 0
      naturalHeight = readable ? 8 : 0
      onload: (() => void) | null = null
      onerror: (() => void) | null = null
      set src(_value: string) {
        queueMicrotask(() => {
          if (readable) this.onload?.()
          else this.onerror?.()
        })
      }
    },
  )
}

/**
 * Dispatch the `change` a browser fires when a file is picked.
 *
 * Not `fireEvent.change`: `@testing-library/preact` re-maps that to React's
 * `onChange` semantics (an `input` event), and a file input never fires
 * `input` — so the handler under test would simply never run and the
 * assertion would pass or fail for the wrong reason. `fireEvent(el, event)`
 * dispatches the real event while keeping the `act()` wrapper.
 */
function pickFile(input: HTMLInputElement): void {
  fireEvent(input, new Event('change', { bubbles: true }))
}

/**
 * Open the editor. Edit stays disabled until the power-levels read lands, and
 * a click on a disabled button is silently a no-op — so wait for the gate to
 * actually open rather than clicking into the void.
 */
async function openEditor(view: {
  findByRole: RenderResult['findByRole']
  findByLabelText: RenderResult['findByLabelText']
}) {
  const button = await view.findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(button.hasAttribute('disabled')).toBe(false))
  fireEvent.click(button)
  await view.findByLabelText('Name')
  return button
}

it('cautions below state_default but still allows editing', async () => {
  server.use(...handlers({ myLevel: 100 }))
  const admin = renderPanel(FIRST)
  const enabled = await admin.findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(enabled.hasAttribute('disabled')).toBe(false))
  cleanup()

  server.resetHandlers()
  server.use(...handlers({ myLevel: 10 }))
  const member = renderPanel(FIRST)
  const cautioned = await member.findByRole('button', { name: 'Edit' })
  await waitFor(() =>
    expect(document.body.textContent).toContain('appears as 10'),
  )
  // Cautioned, not blocked: the thresholds cannot see a room-version-12
  // creator, who holds infinite power yet never appears in `users`.
  expect(cautioned.hasAttribute('disabled')).toBe(false)
  expect(document.body.textContent).toContain('homeserver decides')
})

it('lets a room-version-12 creator edit despite reading as level 0', async () => {
  // From room v12 a creator's power is infinite and they cannot be listed in
  // `users` at all, so `users` is empty and the thresholds resolve them to
  // `users_default`. Blocking on that locked a room's own owner out.
  server.use(...handlers({ users: {} }))
  const view = renderPanel(FIRST)
  const edit = await view.findByRole('button', { name: 'Edit' })
  // Wait for the levels to actually land: before they do, nothing is gated
  // on them and this would pass without proving anything.
  await waitFor(() =>
    expect(document.body.textContent).toContain('appears as 0'),
  )
  expect(edit.hasAttribute('disabled')).toBe(false)
  fireEvent.click(edit)
  expect(await view.findByLabelText('Name')).toBeTruthy()
})

it('writes only the fields that actually changed', async () => {
  const calls: string[] = []
  const bodies: Record<string, unknown> = {}
  server.use(
    ...handlers(),
    http.put(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/name`,
      async ({ request }) => {
        // Record the body *before* announcing the call: a waiter on `calls`
        // would otherwise be free to run while this is still awaiting.
        bodies.name = await request.json()
        calls.push('name')
        return HttpResponse.json({ data: {} })
      },
    ),
    http.put(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/topic`,
      () => {
        calls.push('topic')
        return HttpResponse.json({ data: {} })
      },
    ),
  )
  const view = renderPanel(FIRST)
  const { getByRole, getByLabelText } = view
  await openEditor(view)

  fireEvent.input(getByLabelText('Name'), { target: { value: 'Renamed' } })
  fireEvent.click(getByRole('button', { name: 'Save' }))

  await waitFor(() => expect(calls).toEqual(['name']))
  expect(bodies.name).toEqual({ name: 'Renamed' })
  // The untouched topic must not be rewritten — an empty topic PUT would
  // clear a topic the user never touched.
  expect(calls).not.toContain('topic')
})

it('reports a forbidden save instead of claiming success', async () => {
  server.use(
    ...handlers(),
    http.put(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/name`, () =>
      HttpResponse.json(
        { error: { code: 'forbidden', message: 'nope' } },
        { status: 403 },
      ),
    ),
  )
  const view = renderPanel(FIRST)
  const { findByRole, getByRole, getByLabelText } = view
  await openEditor(view)
  fireEvent.input(getByLabelText('Name'), { target: { value: 'Renamed' } })
  fireEvent.click(getByRole('button', { name: 'Save' }))

  const alert = await findByRole('alert')
  expect(alert.textContent).toContain('Could not save')
  expect(alert.textContent).toContain('not allowed')
  // Still editing: the user keeps their typing rather than losing it.
  expect(getByLabelText('Name')).toBeTruthy()
})

it('refuses a non-image avatar without sending anything', async () => {
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { findByRole, getByRole, container } = view
  await openEditor(view)

  const picker = container.querySelector<HTMLInputElement>('input[type="file"]')
  expect(picker).toBeTruthy()
  const file = new File(['not an image'], 'a.txt', { type: 'text/plain' })
  Object.defineProperty(picker, 'files', { value: [file] })
  pickFile(picker as HTMLInputElement)

  // No upload handler is registered; msw would error on a request, and the
  // message must name the actual reason rather than a generic failure.
  const alert = await findByRole('alert')
  expect(alert.textContent).toContain('must be an image')
  expect(getByRole('button', { name: 'Save' }).hasAttribute('disabled')).toBe(
    true,
  )
})

it('confirms before discarding unsaved changes on close', async () => {
  const onClose = vi.fn()
  server.use(...handlers())
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, FIRST)
  const { findByRole, findByLabelText, getByRole } = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={FIRST}
          room={room(FIRST, 'First')}
          roomTitles={new Map()}
          members={members}
          onClose={onClose}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  const edit = await findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(edit.hasAttribute('disabled')).toBe(false))
  fireEvent.click(edit)
  fireEvent.input(await findByLabelText('Name'), {
    target: { value: 'Renamed' },
  })

  fireEvent.click(getByRole('button', { name: 'Close' }))
  expect(onClose).not.toHaveBeenCalled()
  await findByRole('dialog', { name: 'Discard unsaved room settings' })

  fireEvent.click(getByRole('button', { name: 'Discard changes' }))
  expect(onClose).toHaveBeenCalled()
})

it('drops edit state when the panel moves to another room', async () => {
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { getByLabelText, queryByLabelText, showRoom } = view
  await openEditor(view)
  fireEvent.input(getByLabelText('Name'), { target: { value: 'Renamed' } })

  showRoom(SECOND)
  // Without the room-keyed edit state this would still be open, holding a
  // rename typed for the previous room and ready to save it onto this one.
  await waitFor(() => expect(queryByLabelText('Name')).toBeNull())
})

it('refuses a file that is named like an image but does not decode', async () => {
  // `file.type` comes from the extension, so this arrives as a valid
  // `image/jpeg` and clears every cheap check — the server accepts these too.
  stubImageDecoding(false)
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { findByRole, getByRole, container } = view
  await openEditor(view)

  const picker = container.querySelector<HTMLInputElement>('input[type="file"]')
  const file = new File(['definitely not a jpeg'], 'holiday.jpg', {
    type: 'image/jpeg',
  })
  Object.defineProperty(picker, 'files', { value: [file] })
  pickFile(picker as HTMLInputElement)

  const alert = await findByRole('alert')
  expect(alert.textContent).toContain('holiday.jpg')
  expect(alert.textContent).toContain('could not be read')
  // Nothing staged, so there is nothing to save and no request was made.
  expect(getByRole('button', { name: 'Save' }).hasAttribute('disabled')).toBe(
    true,
  )
})

it('accepts a file whose bytes do decode', async () => {
  stubImageDecoding(true)
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { getByRole, container, queryByRole } = view
  await openEditor(view)

  const picker = container.querySelector<HTMLInputElement>('input[type="file"]')
  const file = new File(['real bytes'], 'holiday.jpg', { type: 'image/jpeg' })
  Object.defineProperty(picker, 'files', { value: [file] })
  pickFile(picker as HTMLInputElement)

  await waitFor(() =>
    expect(getByRole('button', { name: 'Save' }).hasAttribute('disabled')).toBe(
      false,
    ),
  )
  expect(queryByRole('alert')).toBeNull()
})

/** A drop carrying `files`, as a browser delivers it. */
function dropFiles(target: Element, files: File[]): void {
  const event = new Event('drop', { bubbles: true }) as DragEvent
  Object.defineProperty(event, 'dataTransfer', {
    value: { files, types: ['Files'] },
  })
  fireEvent(target, event)
}

/**
 * A paste carrying `files`, as a browser delivers a copied image. Returns the
 * event so a test can read `defaultPrevented` — the only observable signal
 * for whether the handler consumed the paste or let it fall through to the
 * focused field.
 */
function pasteFiles(target: Element, files: File[]): ClipboardEvent {
  const event = new Event('paste', {
    bubbles: true,
    cancelable: true,
  }) as ClipboardEvent
  Object.defineProperty(event, 'clipboardData', {
    value: { files, items: [] },
  })
  fireEvent(target, event)
  return event
}

it('accepts an avatar dropped onto the form', async () => {
  stubImageDecoding(true)
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { getByRole, container } = view
  await openEditor(view)

  const form = container.querySelector('.room-settings-form')
  dropFiles(form as Element, [
    new File(['bytes'], 'a.png', { type: 'image/png' }),
  ])

  await waitFor(() =>
    expect(getByRole('button', { name: 'Save' }).hasAttribute('disabled')).toBe(
      false,
    ),
  )
})

it('runs a dropped file through the same checks as the picker', async () => {
  // The point of routing every source through one entry point: a drop must
  // not be a way to skip the decode the picker does.
  stubImageDecoding(false)
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { findByRole, getByRole, container } = view
  await openEditor(view)

  const form = container.querySelector('.room-settings-form')
  dropFiles(form as Element, [
    new File(['nope'], 'holiday.jpg', { type: 'image/jpeg' }),
  ])

  const alert = await findByRole('alert')
  expect(alert.textContent).toContain('could not be read')
  expect(getByRole('button', { name: 'Save' }).hasAttribute('disabled')).toBe(
    true,
  )
})

it('accepts a pasted image but leaves a text paste alone', async () => {
  stubImageDecoding(true)
  server.use(...handlers())
  const view = renderPanel(FIRST)
  const { getByRole, getByLabelText, container } = view
  await openEditor(view)

  const form = container.querySelector('.room-settings-form') as Element

  // A text paste carries no image: it must fall through to the field the user
  // is typing in rather than being swallowed by the avatar handler. Leaving
  // the event uncancelled is exactly what "falls through" means here.
  const textPaste = pasteFiles(form, [])
  expect(textPaste.defaultPrevented).toBe(false)
  fireEvent.input(getByLabelText('Name'), { target: { value: 'Typed' } })
  expect((getByLabelText('Name') as HTMLInputElement).value).toBe('Typed')

  const imagePaste = pasteFiles(form, [
    new File(['bytes'], 'shot.png', { type: 'image/png' }),
  ])
  expect(imagePaste.defaultPrevented).toBe(true)
  await waitFor(() =>
    expect(getByRole('button', { name: 'Save' }).hasAttribute('disabled')).toBe(
      false,
    ),
  )
})

it('opens the avatar at full size, and only when there is one', async () => {
  server.use(...handlers())
  // No avatar: the fallback is a coloured letter, so there is nothing to open.
  const plain = renderPanel(FIRST)
  await plain.findByRole('button', { name: 'Edit' })
  expect(
    plain.queryByRole('button', { name: /View the .* avatar at full size/ }),
  ).toBeNull()
  cleanup()

  server.resetHandlers()
  server.use(...handlers())
  const withAvatar = renderPanel(FIRST, { avatar_url: 'mxc://hs/pic' })
  const open = await withAvatar.findByRole('button', {
    name: /View the .* avatar at full size/,
  })
  fireEvent.click(open)
  expect(
    await withAvatar.findByRole('dialog', { name: /First avatar/ }),
  ).toBeTruthy()
})

it('routes a replace from the viewer through the form, not straight to a write', async () => {
  stubImageDecoding(true)
  server.use(...handlers())
  const view = renderPanel(FIRST, { avatar_url: 'mxc://hs/pic' })
  // The replace control is gated on the power-levels read, so wait for the
  // gate to resolve before opening the viewer rather than racing it.
  const edit = await view.findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(edit.hasAttribute('disabled')).toBe(false))
  const open = view.getByRole('button', {
    name: /View the .* avatar at full size/,
  })
  fireEvent.click(open)
  await view.findByRole('dialog', { name: /First avatar/ })

  const picker = document.querySelector<HTMLInputElement>(
    '.room-info-avatar-replace input[type="file"]',
  )
  expect(picker).toBeTruthy()
  Object.defineProperty(picker, 'files', {
    value: [new File(['bytes'], 'new.png', { type: 'image/png' })],
  })
  pickFile(picker as HTMLInputElement)

  // The viewer closes and the edit form opens with the image staged — no
  // request has gone out, and Save is what commits it. No PUT handler is
  // registered here, so an immediate write would fail loudly.
  await waitFor(() =>
    expect(
      view.getByRole('button', { name: 'Save' }).hasAttribute('disabled'),
    ).toBe(false),
  )
  expect(view.getByLabelText('Name')).toBeTruthy()
})

it('still offers the viewer replace control below the threshold', async () => {
  // Same reasoning as the Edit button: a room-version-12 creator reads as
  // level 0 here, and hiding the control would strand them.
  server.use(...handlers({ myLevel: 10 }))
  const view = renderPanel(FIRST, { avatar_url: 'mxc://hs/pic' })
  await waitFor(() =>
    expect(document.body.textContent).toContain('appears as 10'),
  )
  fireEvent.click(
    view.getByRole('button', { name: /View the .* avatar at full size/ }),
  )
  await view.findByRole('dialog', { name: /First avatar/ })

  expect(document.querySelector('.room-info-avatar-replace')).toBeTruthy()
})

it('does not write back a field someone else changed while the editor was open', async () => {
  const puts: string[] = []
  server.use(
    ...handlers(),
    http.put(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/name`,
      () => {
        puts.push('name')
        return HttpResponse.json({ data: {} })
      },
    ),
    http.put(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/topic`,
      () => {
        puts.push('topic')
        return HttpResponse.json({ data: {} })
      },
    ),
  )
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, FIRST)
  const render1 = (name: string) => (
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={FIRST}
          room={room(FIRST, name)}
          roomTitles={new Map()}
          members={members}
          onClose={() => {}}
        />
      </LocationProvider>
    </ServicesContext.Provider>
  )
  const view = render(render1('First'))
  const edit = await view.findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(edit.hasAttribute('disabled')).toBe(false))
  fireEvent.click(edit)
  await view.findByLabelText('Name')

  // Another client renames the room while this editor is open; the live patch
  // flows in through the `room` prop.
  view.rerender(render1('Renamed Elsewhere'))

  // The user edits only the topic and saves.
  fireEvent.input(view.getByLabelText('Topic'), {
    target: { value: 'my topic' },
  })
  fireEvent.click(view.getByRole('button', { name: 'Save' }))

  await waitFor(() => expect(puts).toContain('topic'))
  // Measured against the live prop, the untouched name field would look dirty
  // and Save would write the pre-rename value back, reverting the other
  // client's change.
  expect(puts).not.toContain('name')
})

it('stops offering to discard once the save is in flight', async () => {
  const onClose = vi.fn()
  let releaseName: (() => void) | undefined
  const held = new Promise<void>((resolve) => {
    releaseName = resolve
  })
  server.use(
    ...handlers(),
    http.put(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/name`,
      async () => {
        await held
        return HttpResponse.json({ data: {} })
      },
    ),
  )
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, FIRST)
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={FIRST}
          room={room(FIRST, 'First')}
          roomTitles={new Map()}
          members={members}
          onClose={onClose}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  const edit = await view.findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(edit.hasAttribute('disabled')).toBe(false))
  fireEvent.click(edit)
  fireEvent.input(await view.findByLabelText('Name'), {
    target: { value: 'Renamed' },
  })
  fireEvent.click(view.getByRole('button', { name: 'Save' }))

  // The requests are out: the changes are not unsaved, so a discard prompt
  // would be describing something that has already happened, and closing
  // does not cancel them.
  fireEvent.click(view.getByRole('button', { name: 'Close' }))
  expect(
    view.queryByRole('dialog', { name: 'Discard unsaved room settings' }),
  ).toBeNull()
  expect(onClose).toHaveBeenCalled()

  releaseName?.()
})

it('does not treat a DM peer picture as the room avatar', async () => {
  stubImageDecoding(true)
  const DM = '!dm:hs'
  server.use(
    // Registered before the shared factory: msw takes the first match, and
    // `handlers()` answers `/members` with an empty list.
    // An unnamed two-person room: the store resolves its title and peer
    // avatar from members, which is what populates `dmAvatars`.
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({
        data: [
          {
            account_id: ACCOUNT,
            account_user_id: '@me:example.org',
            room_id: DM,
            last_activity_ts: 1,
            notification_count: 0,
            highlight_count: 0,
          },
        ],
      }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
      () =>
        HttpResponse.json({
          data: [
            {
              user_id: '@me:example.org',
              membership: 'join',
              display_name: 'Me',
            },
            {
              user_id: '@bob:example.org',
              membership: 'join',
              display_name: 'Bob',
              avatar_url: 'mxc://hs/bob',
            },
          ],
        }),
    ),
    http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:id`, () =>
      HttpResponse.arrayBuffer(new ArrayBuffer(8), {
        headers: { 'content-type': 'image/png' },
      }),
    ),
    ...handlers(),
  )
  const services = testServices()
  await services.rooms.refresh()
  await waitFor(() => expect(services.rooms.dmAvatars.value.size).toBe(1))

  const members = createMembersStore(services.api, ACCOUNT, DM)
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={DM}
          room={services.rooms.rooms.value[0]}
          roomTitles={services.rooms.titles.value}
          members={members}
          onClose={() => {}}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )

  // The panel shows the peer's picture, as the room list does.
  await waitFor(() =>
    expect(
      view.container.querySelector('.room-info-identity .room-avatar'),
    ).toBeTruthy(),
  )
  const edit = await view.findByRole('button', { name: 'Edit' })
  await waitFor(() => expect(edit.hasAttribute('disabled')).toBe(false))
  fireEvent.click(edit)
  await view.findByLabelText('Name')

  // But the room itself has no `m.room.avatar`, so there is nothing to
  // remove: a DELETE would clear an already-absent avatar and leave the
  // peer's picture on screen, looking like the button did nothing.
  expect(
    view
      .getByRole('button', { name: 'Remove avatar' })
      .hasAttribute('disabled'),
  ).toBe(true)
  // And the editor says why the picture it was showing is not in the form.
  expect(view.container.textContent).toContain('other member')
})

/** Render the panel with a controllable room, for navigation-style rerenders. */
function renderSwitchable(onClose = () => {}) {
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, FIRST)
  const tree = (roomId: string, name: string) => (
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={roomId}
          room={room(roomId, name)}
          roomTitles={new Map()}
          members={members}
          onClose={onClose}
        />
      </LocationProvider>
    </ServicesContext.Provider>
  )
  const view = render(tree(FIRST, 'First'))
  return {
    ...view,
    show: (id: string, n: string) => view.rerender(tree(id, n)),
  }
}

it('confirms before a member DM discards an unsaved edit', async () => {
  server.use(
    // Overrides go before the factory: msw takes the first match, and
    // `handlers()` answers `/members` with an empty list.
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
      () =>
        HttpResponse.json({
          data: [
            {
              user_id: '@bob:example.org',
              membership: 'join',
              display_name: 'Bob',
            },
          ],
        }),
    ),
    ...handlers(),
  )
  // The panel does not fetch members itself — `RoomPage` owns that store and
  // populates it — so seed one here rather than rendering an empty roster.
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, FIRST)
  await members.refresh()
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={FIRST}
          room={room(FIRST, 'First')}
          roomTitles={new Map()}
          members={members}
          onClose={() => {}}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  const dm = await view.findByRole('button', { name: /Open DM with Bob/ })

  await openEditor(view)
  fireEvent.input(view.getByLabelText('Name'), { target: { value: 'Renamed' } })
  fireEvent.click(dm)

  // Starting a DM navigates away and then closes the panel, so the guard has
  // to run before the action, not at the onClose it ends with.
  await view.findByRole('dialog', { name: 'Discard unsaved room settings' })
})

it('confirms before opening a related room discards an unsaved edit', async () => {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/parents`,
      () =>
        HttpResponse.json({
          data: [
            {
              room_id: '!parent:hs',
              room_type: 'm.space',
              name: 'Parent Space',
              canonical: false,
              via: [],
            },
          ],
        }),
    ),
    ...handlers(),
  )
  const view = renderPanel(FIRST)
  await openEditor(view)
  fireEvent.input(view.getByLabelText('Name'), { target: { value: 'Renamed' } })

  fireEvent.click(
    await view.findByRole('button', { name: /Parent: Parent Space/ }),
  )
  await view.findByRole('dialog', { name: 'Discard unsaved room settings' })

  // Confirming resumes what it interrupted rather than merely closing.
  fireEvent.click(view.getByRole('button', { name: 'Discard changes' }))
  await waitFor(() => expect(view.queryByLabelText('Name')).toBeNull())
})

it('finishes a save cleanly even when its form is gone', async () => {
  let release: (() => void) | undefined
  const held = new Promise<void>((resolve) => {
    release = resolve
  })
  let refreshes = 0
  const onClose = vi.fn()
  server.use(
    http.put(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/name`,
      async () => {
        await held
        return HttpResponse.json({ data: {} })
      },
    ),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () => {
      refreshes += 1
      return HttpResponse.json({ data: [] })
    }),
    ...handlers(),
  )
  const view = renderSwitchable(onClose)
  fireEvent.click(await view.findByRole('button', { name: 'Edit' }))
  fireEvent.input(await view.findByLabelText('Name'), {
    target: { value: 'Renamed' },
  })
  fireEvent.click(view.getByRole('button', { name: 'Save' }))

  // Switch rooms while the PUT is still in flight, then let it land.
  view.show(SECOND, 'Second')
  await waitFor(() => expect(view.queryByLabelText('Name')).toBeNull())
  release?.()

  // The socket-down fallback refresh must still run: the write succeeded, so
  // the room list should reflect it whoever is on screen by now. It is also
  // the signal that the save has finished unwinding.
  await waitFor(() => expect(refreshes).toBeGreaterThan(0))

  // Now edit the *new* room and close. If the finished save left the
  // parent's `settingsSaving` stuck true, the discard guard is skipped and
  // this second edit vanishes with no confirmation.
  fireEvent.click(view.getByRole('button', { name: 'Edit' }))
  fireEvent.input(await view.findByLabelText('Name'), {
    target: { value: 'Second Rename' },
  })
  fireEvent.click(view.getByRole('button', { name: 'Close' }))

  await view.findByRole('dialog', { name: 'Discard unsaved room settings' })
  expect(onClose).not.toHaveBeenCalled()
})

it('does not carry a saved-status banner into another room', async () => {
  server.use(
    ...handlers(),
    http.put(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/name`, () =>
      HttpResponse.json({ data: {} }),
    ),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: [] }),
    ),
  )
  const view = renderSwitchable()
  fireEvent.click(await view.findByRole('button', { name: 'Edit' }))
  fireEvent.input(await view.findByLabelText('Name'), {
    target: { value: 'Renamed' },
  })
  fireEvent.click(view.getByRole('button', { name: 'Save' }))
  await view.findByText(/Saved name\./)

  view.show(SECOND, 'Second')
  // Nothing was saved in this room; claiming otherwise misattributes it.
  await waitFor(() => expect(view.queryByText(/Saved name\./)).toBeNull())
})
