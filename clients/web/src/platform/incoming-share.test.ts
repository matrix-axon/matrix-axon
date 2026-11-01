import { beforeEach, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'
import { nativeIncomingShare } from './incoming-share'
import manifest from '../incoming-share/fixtures/manifest-v1.json'
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
beforeEach(() => vi.mocked(invoke).mockReset())
it('accepts a valid device preview manifest', async () => {
  vi.mocked(invoke).mockResolvedValue({ value: manifest })
  await expect(nativeIncomingShare().call('device_preview')).resolves.toEqual(
    manifest,
  )
})
it.each([null, {}, { version: 1, text: 'incomplete' }])(
  'rejects malformed device preview manifests',
  async (value) => {
    vi.mocked(invoke).mockResolvedValue({ value })
    await expect(nativeIncomingShare().call('device_preview')).rejects.toThrow(
      'Incoming share: invalid',
    )
  },
)
