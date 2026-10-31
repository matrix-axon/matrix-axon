import { act, render, screen, waitFor } from '@testing-library/preact'
import { expect, it, vi } from 'vitest'
import type { ApiClient } from '../api/client'
import { useImagePackCompletions } from './use-image-pack-completions'

function setup() {
  const get = vi.fn(async () => ({
    data: {
      data: {
        partial: false,
        packs: [
          {
            room_id: null,
            state_key: null,
            event_type: 'im.ponies.user_emotes',
            images: [
              {
                shortcode: 'party',
                url: 'mxc://hs/party',
                emoticon: true,
                sticker: true,
              },
            ],
          },
        ],
      },
    },
    response: { status: 200 },
  }))
  const api = { GET: get } as unknown as ApiClient
  function Suggestions({
    query,
    room = 'room',
    usage = 'emoticon',
  }: {
    query: string | null
    room?: string
    usage?: 'emoticon' | 'sticker'
  }) {
    const matches = useImagePackCompletions(
      { api, accountId: 'account', roomId: room },
      query,
      usage,
    )
    return (
      <div data-testid="matches">{matches.map((m) => m.value).join(',')}</div>
    )
  }
  return { get, Suggestions }
}

it('reuses a bounded scope snapshot across separate prose queries and usages', async () => {
  const { get, Suggestions } = setup()
  const view = render(<Suggestions query="pa" />)
  await waitFor(() =>
    expect(screen.getByTestId('matches').textContent).toBe(':party:'),
  )
  view.rerender(<Suggestions query={null} />)
  await act(async () => {})
  view.rerender(<Suggestions query="part" />)
  await waitFor(() =>
    expect(screen.getByTestId('matches').textContent).toBe(':party:'),
  )
  expect(get).toHaveBeenCalledTimes(1)
  view.rerender(<Suggestions query="pa" usage="sticker" />)
  await waitFor(() => expect(get).toHaveBeenCalledTimes(2))
  view.rerender(<Suggestions query="pa" />)
  await waitFor(() =>
    expect(screen.getByTestId('matches').textContent).toBe(':party:'),
  )
  expect(get).toHaveBeenCalledTimes(2)
  view.rerender(<Suggestions query="pa" room="other" />)
  await waitFor(() => expect(get).toHaveBeenCalledTimes(3))
})

it('refreshes an expired snapshot on the next query activation', async () => {
  const { get, Suggestions } = setup()
  const now = vi.spyOn(Date, 'now').mockReturnValue(1000)
  try {
    const view = render(<Suggestions query="pa" />)
    await waitFor(() =>
      expect(screen.getByTestId('matches').textContent).toBe(':party:'),
    )
    view.rerender(<Suggestions query={null} />)
    await act(async () => {})
    now.mockReturnValue(32000)
    view.rerender(<Suggestions query="pa" />)
    await waitFor(() => expect(get).toHaveBeenCalledTimes(2))
  } finally {
    now.mockRestore()
  }
})
