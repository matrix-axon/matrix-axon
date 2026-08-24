import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { CopyableText } from './CopyableText'

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

describe('CopyableText', () => {
  it('copies the value and announces success', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { getByRole, findByRole } = render(
      <CopyableText text="!room:hs" label="Room ID" />,
    )

    fireEvent.click(getByRole('button', { name: 'Copy Room ID' }))
    await waitFor(() => expect(writeText).toHaveBeenCalledWith('!room:hs'))
    expect((await findByRole('status')).textContent).toBe('Copied')
  })

  it('announces failure when the write is rejected', async () => {
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) },
    })
    const { getByRole, findByRole } = render(
      <CopyableText text="abc" label="version" />,
    )

    fireEvent.click(getByRole('button', { name: 'Copy version' }))
    expect((await findByRole('status')).textContent).toBe('Copy failed')
  })
})
