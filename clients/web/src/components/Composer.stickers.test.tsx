import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { createApiClient } from '../api/client'
import { Composer } from './Composer'

// Media loading is covered in real-browser autocomplete tests.
vi.mock('./ReactionContent', () => ({ ReactionContent: () => null }))

const BASE = 'http://axon.test'
const PACKS = `${BASE}/v1/accounts/:accountId/rooms/:roomId/emoji-packs`
const api = createApiClient(
  {
    getToken: () => 'test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  },
  BASE,
)
const image = {
  shortcode: 'adam',
  url: 'mxc://hs/adam',
  body: 'Adam',
  info: { w: 200, h: 100 },
  sticker: true,
  emoticon: false,
}
const pack = (room_id: string | null, url = image.url) => ({
  room_id,
  state_key: room_id ? 'Testing' : null,
  event_type: room_id ? 'm.room.image_pack' : 'im.ponies.user_emotes',
  images: [{ ...image, url }],
})
const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function setup(extra: Partial<Parameters<typeof Composer>[0]> = {}) {
  const onSelect = vi.fn()
  const onSubmit = vi.fn(async () => true)
  const onDraftChange = vi.fn()
  const view = render(
    <Composer
      placeholder="Message"
      initialValue=":adam:"
      onSubmit={onSubmit}
      onDraftChange={onDraftChange}
      stickers={{ api, accountId: 'account', roomId: '!room:hs', onSelect }}
      {...extra}
    />,
  )
  const send = screen.getByRole('button', { name: 'Send', exact: true })
  const input = screen.getByRole('textbox') as HTMLTextAreaElement
  return { ...view, onSelect, onSubmit, onDraftChange, send, input }
}
function servePacks() {
  server.use(
    http.get(PACKS, ({ request }) => {
      expect(new URL(request.url).searchParams.get('usage')).toBe('sticker')
      return HttpResponse.json({
        data: {
          packs: [pack(null), pack('!room:hs', 'mxc://hs/room-adam')],
          partial: false,
        },
      })
    }),
  )
}

it('submits the first matching sticker through the same callback as picker selection', async () => {
  servePacks()
  const { send, input, onSelect, onSubmit, onDraftChange } = setup({
    initialValue: ':AdAm:',
  })
  expect(onSelect).not.toHaveBeenCalled()
  fireEvent.click(send)
  expect(send.hasAttribute('disabled')).toBe(true)
  await waitFor(() => expect(onSelect).toHaveBeenCalledOnce())
  expect(onSelect).toHaveBeenCalledWith(image)
  expect(onSubmit).not.toHaveBeenCalled()
  expect(input.value).toBe('')
  expect(onDraftChange).toHaveBeenCalledWith('')
})

it.each([404, 200])(
  'retains text/Unicode handling for an unavailable or unknown name (%s)',
  async (status) => {
    server.use(
      http.get(PACKS, () =>
        HttpResponse.json(
          status === 200 ? { data: { packs: [], partial: false } } : {},
          { status },
        ),
      ),
    )
    const { send, input, onSelect, onSubmit } = setup()
    fireEvent.click(send)
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(':adam:'))
    expect(onSelect).not.toHaveBeenCalled()
    expect(input.value).toBe('')
  },
)

it.each(['Hello :adam:', '` :adam: `', ':adam: :adam:', '\\:adam:'])(
  'keeps prose and escaped tokens as text: %s',
  async (initialValue) => {
    const { send, onSelect, onSubmit } = setup({ initialValue })
    fireEvent.click(send)
    expect(onSubmit).toHaveBeenCalledWith(initialValue)
    expect(onSelect).not.toHaveBeenCalled()
  },
)

it('does not reinterpret attachment captions or edit-mode composers', () => {
  const file = new File(['x'], 'test.png', { type: 'image/png' })
  const first = setup({
    attachments: {
      items: [{ id: '1', file, previewUrl: null }],
      skipped: 0,
      skippedReason: null,
      totalBytes: 1,
      onRemove: vi.fn(),
    },
  })
  fireEvent.click(first.send)
  expect(first.onSubmit).toHaveBeenCalledWith(':adam:')
  first.unmount()
  const edit = setup({ stickers: undefined })
  fireEvent.click(edit.send)
  expect(edit.onSubmit).toHaveBeenCalledWith(':adam:')
})

it('keeps the draft on a discovery failure and allows an explicit retry', async () => {
  server.use(http.get(PACKS, () => HttpResponse.json({}, { status: 503 })))
  const { send, input, onSelect, onSubmit } = setup()
  fireEvent.click(send)
  await screen.findByRole('alert')
  expect(input.value).toBe(':adam:')
  expect(onSelect).not.toHaveBeenCalled()
  expect(onSubmit).not.toHaveBeenCalled()
  await waitFor(() => expect(send.hasAttribute('disabled')).toBe(false))
  servePacks()
  fireEvent.click(send)
  await waitFor(() => expect(onSelect).toHaveBeenCalledOnce())
  expect(input.value).toBe('')
})

it('deduplicates pending submits and preserves a newer draft', async () => {
  let release!: () => void
  const ready = new Promise<void>((resolve) => {
    release = resolve
  })
  server.use(
    http.get(PACKS, async () => {
      await ready
      return HttpResponse.json({
        data: { packs: [pack(null)], partial: false },
      })
    }),
  )
  const { send, input, onSelect } = setup()
  fireEvent.click(send)
  fireEvent.keyDown(input, { key: 'Enter' })
  fireEvent.input(input, { target: { value: 'Next message' } })
  await act(async () => {
    release()
    await ready
  })
  await waitFor(() => expect(onSelect).toHaveBeenCalledOnce())
  expect(input.value).toBe('Next message')
})

it('does not send a delayed lookup after the composer is unmounted', async () => {
  let release!: () => void
  const ready = new Promise<void>((resolve) => {
    release = resolve
  })
  server.use(
    http.get(PACKS, async () => {
      await ready
      return HttpResponse.json({
        data: { packs: [pack(null)], partial: false },
      })
    }),
  )
  const { send, unmount, onSelect, onSubmit } = setup()
  fireEvent.click(send)
  unmount()
  await act(async () => {
    release()
    await ready
  })
  expect(onSelect).not.toHaveBeenCalled()
  expect(onSubmit).not.toHaveBeenCalled()
})

it('suggests stickers case-insensitively, completes with Tab, and preserves source spelling', async () => {
  servePacks()
  const { input, send, onSelect } = setup({ initialValue: '' })
  fireEvent.input(input, { target: { value: ':AD' } })
  const choice = await screen.findByRole('option', { name: /:adam:.*Sticker/ })
  expect(choice.textContent).toContain('Personal')
  // A room duplicate resolves to the personal image, so show one effective choice.
  expect(screen.getAllByRole('option')).toHaveLength(1)
  fireEvent.keyDown(input, { key: 'Tab' })
  expect(input.value.trim()).toBe(':adam:')
  expect(onSelect).not.toHaveBeenCalled()
  fireEvent.click(send)
  await waitFor(() => expect(onSelect).toHaveBeenCalledWith(image))
})

it('keeps sticker suggestions out of prose and reaction commands', () => {
  const first = setup({ initialValue: 'Hello :AD' })
  expect(screen.queryByRole('option')).toBeNull()
  first.unmount()
  setup({ initialValue: '/react :AD', onCommand: vi.fn() })
  expect(screen.queryByRole('option')).toBeNull()
})
