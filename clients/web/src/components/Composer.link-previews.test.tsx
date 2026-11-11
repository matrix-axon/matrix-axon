import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Composer, type ComposerLinkPreviewOptions } from './Composer'
import {
  createLinkPreviewCache,
  type PreviewSources,
} from '../composer-link-previews'
import { LinkPreviewError, type DeviceLinkPreview } from '../platform'

/** The composer's link-preview cards (ADR 0132 § 5), driven as a user would. */

const A = 'https://a.example.org/x'
const B = 'https://b.example.org/y'

afterEach(() => vi.useRealTimers())

function deferredDevice() {
  const pending: {
    url: string
    signal: AbortSignal
    resolve: (preview: DeviceLinkPreview) => void
    reject: (error: unknown) => void
  }[] = []
  const device = vi.fn(
    (url: string, signal: AbortSignal) =>
      new Promise<DeviceLinkPreview>((resolve, reject) =>
        pending.push({ url, signal, resolve, reject }),
      ),
  )
  return { device, pending }
}

function setup({
  sources,
  initialValue = '',
  markerFits = vi.fn(async () => true),
  attachments,
}: {
  sources: PreviewSources
  initialValue?: string
  markerFits?: ComposerLinkPreviewOptions['markerFits']
  attachments?: Parameters<typeof Composer>[0]['attachments']
}) {
  const onSubmit = vi.fn(async () => true)
  const linkPreviews: ComposerLinkPreviewOptions = {
    sources,
    cache: createLinkPreviewCache(),
    markerFits,
  }
  const utils = render(
    <Composer
      placeholder="Message"
      initialValue={initialValue}
      onSubmit={onSubmit}
      linkPreviews={linkPreviews}
      attachments={attachments}
    />,
  )
  const textarea = utils.getByRole('textbox') as HTMLTextAreaElement
  const type = (value: string) =>
    fireEvent.input(textarea, { target: { value } })
  const paste = (value: string) =>
    fireEvent.input(textarea, {
      target: { value },
      inputType: 'insertFromPaste',
    })
  const send = () => fireEvent.submit(textarea.closest('form')!)
  return { ...utils, onSubmit, textarea, type, paste, send, markerFits }
}

const cards = () =>
  screen.queryByRole('list', { name: 'Link previews to send' })

describe('Composer link previews', () => {
  it('shows a card for a typed link once it is complete, and sends it', async () => {
    vi.useFakeTimers()
    const { device, pending } = deferredDevice()
    const { type, send, onSubmit } = setup({
      sources: { setting: 'device', device },
    })
    type(`read ${A}`)
    expect(cards()).toBeNull()
    type(`read ${A} `)
    expect(screen.getByText('Loading preview…')).toBeTruthy()
    await act(async () => {
      await vi.advanceTimersByTimeAsync(400)
    })
    expect(device).toHaveBeenCalledWith(A, expect.any(AbortSignal))
    pending[0].resolve({ url: A, title: 'An article', imageUnavailable: false })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0)
    })
    expect(screen.getByText('An article')).toBeTruthy()
    expect(screen.getByText('a.example.org')).toBeTruthy()

    send()
    expect(onSubmit).toHaveBeenCalledWith(`read ${A}`, undefined, {
      draft: {
        cards: [
          {
            matchedUrl: A,
            title: 'An article',
            description: null,
            siteName: null,
            image: null,
          },
        ],
        discarded: new Set(),
      },
      allowUnmarked: false,
    })
    // The next draft starts clean.
    expect(cards()).toBeNull()
  })

  it('looks a pasted link up at once', () => {
    const { device } = deferredDevice()
    const { paste } = setup({ sources: { setting: 'device', device } })
    paste(A)
    expect(device).toHaveBeenCalledTimes(1)
  })

  it('discards a card with its X and sends the discard record', async () => {
    const { device, pending } = deferredDevice()
    const { paste, send, onSubmit } = setup({
      sources: { setting: 'device', device },
    })
    paste(`${A} `)
    fireEvent.click(
      screen.getByRole('button', {
        name: 'Remove the preview of a.example.org',
      }),
    )
    expect(pending[0].signal.aborted).toBe(true)
    expect(cards()).toBeNull()
    send()
    // A discarded link in the body waits for the marker check (§ 6).
    await waitFor(() =>
      expect(onSubmit).toHaveBeenCalledWith(A, undefined, {
        draft: { cards: [], discarded: new Set([A]) },
        allowUnmarked: false,
      }),
    )
  })

  it('never looks up a link in a restored draft', async () => {
    vi.useFakeTimers()
    const { device } = deferredDevice()
    const { type } = setup({
      sources: { setting: 'device', device },
      initialValue: `restored ${A} `,
    })
    type(`restored ${A} and more `)
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000)
    })
    expect(device).not.toHaveBeenCalled()
    expect(cards()).toBeNull()
  })

  it('shows no cards while a file is staged', () => {
    const { device } = deferredDevice()
    const { paste } = setup({
      sources: { setting: 'device', device },
      attachments: {
        items: [
          {
            id: '1',
            file: new File(['x'], 'a.txt', { type: 'text/plain' }),
            previewUrl: null,
          },
        ],
        skipped: 0,
        skippedReason: null,
        totalBytes: 1,
        onRemove: () => {},
      },
    })
    paste(A)
    expect(device).not.toHaveBeenCalled()
    expect(cards()).toBeNull()
  })

  it('explains a failed lookup and offers the homeserver only on request', async () => {
    const { device, pending } = deferredDevice()
    const homeserver = vi.fn(async () => ({
      title: 'From the homeserver',
      description: null,
      siteName: null,
      image: null,
      imageLost: false,
    }))
    const { paste } = setup({
      sources: { setting: 'device+homeserver', device, homeserver },
    })
    paste(A)
    pending[0].reject(new LinkPreviewError('public_ipv4_required'))
    await waitFor(() =>
      expect(screen.getByText(/No IPv4 address was found/)).toBeTruthy(),
    )
    expect(homeserver).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole('button', { name: 'Try homeserver' }))
    expect(homeserver).toHaveBeenCalledWith(A, expect.any(AbortSignal))
    await waitFor(() =>
      expect(screen.getByText('From the homeserver')).toBeTruthy(),
    )
  })

  it('starts a browser card at the homeserver offer', () => {
    const homeserver = vi.fn()
    const { paste } = setup({
      sources: { setting: 'device+homeserver', homeserver },
    })
    paste(A)
    expect(screen.getByText(/It will see the link/)).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Try homeserver' })).toBeTruthy()
    expect(homeserver).not.toHaveBeenCalled()
  })

  it('asks before sending a message too long for its marker, keeping the draft', async () => {
    const { device } = deferredDevice()
    const markerFits = vi.fn(async () => false)
    const { paste, send, onSubmit, textarea } = setup({
      sources: { setting: 'device', device },
      markerFits,
    })
    paste(`${A} ${B} `)
    fireEvent.click(
      screen.getByRole('button', {
        name: 'Remove the preview of a.example.org',
      }),
    )
    send()
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy())
    expect(markerFits).toHaveBeenCalledWith(`${A} ${B}`, undefined, {
      cards: [],
      discarded: new Set([A]),
    })
    expect(onSubmit).not.toHaveBeenCalled()
    expect(textarea.value).toBe(`${A} ${B} `)

    fireEvent.click(screen.getByRole('button', { name: 'Send anyway' }))
    expect(onSubmit).toHaveBeenCalledWith(`${A} ${B}`, undefined, {
      draft: { cards: [], discarded: new Set([A]) },
      allowUnmarked: true,
    })
    expect(textarea.value).toBe('')
  })

  it('sends without asking when the marker fits', async () => {
    const { device } = deferredDevice()
    const { paste, send, onSubmit } = setup({
      sources: { setting: 'device', device },
    })
    paste(`${A} `)
    fireEvent.click(
      screen.getByRole('button', {
        name: 'Remove the preview of a.example.org',
      }),
    )
    send()
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1))
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('does not ask about a message with no discarded link', () => {
    const { device } = deferredDevice()
    const markerFits = vi.fn(async () => false)
    const { paste, send, onSubmit } = setup({
      sources: { setting: 'device', device },
      markerFits,
    })
    paste(`${A} `)
    send()
    expect(markerFits).not.toHaveBeenCalled()
    expect(onSubmit).toHaveBeenCalledTimes(1)
  })

  it('makes no cards when previews are off', () => {
    const { device } = deferredDevice()
    const { paste, send, onSubmit } = setup({
      sources: { setting: 'off', device },
    })
    paste(`${A} `)
    expect(device).not.toHaveBeenCalled()
    send()
    // Still a preview-authoring composer: the parent decides the field.
    expect(onSubmit).toHaveBeenCalledWith(A, undefined, {
      draft: { cards: [], discarded: new Set() },
      allowUnmarked: false,
    })
  })
})
