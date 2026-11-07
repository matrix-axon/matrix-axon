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

it('preserves photo sizing in a native recovery manifest and rejects malformed choices', async () => {
  const sized = structuredClone(manifest)
  sized.version = 2
  Object.assign(sized.files[2], {
    mime: 'image/heic',
    photoSize: 'small',
    photoBytes: 512,
  })
  vi.mocked(invoke).mockResolvedValue({ value: sized })
  await expect(nativeIncomingShare().call('claim')).resolves.toEqual(sized)
  for (const fields of [
    { photoSize: 'huge' },
    { photoSize: 'small', photoBytes: -1 },
    { photoSize: 'small', photoBytes: undefined },
  ]) {
    const invalid = structuredClone(sized)
    Object.assign(invalid.files[2], fields)
    vi.mocked(invoke).mockResolvedValue({ value: invalid })
    await expect(nativeIncomingShare().call('claim')).rejects.toThrow(
      'Incoming share: invalid',
    )
  }
})

it('accepts version 3 captions and rejects captions older consumers would ignore', async () => {
  const captions = structuredClone(manifest)
  captions.version = 3
  Object.assign(captions.files[2], {
    caption: 'Second café 🌊',
    mime: 'image/png',
  })
  vi.mocked(invoke).mockResolvedValue({ value: captions })
  await expect(nativeIncomingShare().call('claim')).resolves.toEqual(captions)
  for (const version of [1, 2, 4]) {
    vi.mocked(invoke).mockResolvedValue({ value: { ...captions, version } })
    await expect(nativeIncomingShare().call('claim')).rejects.toThrow(
      'Incoming share: invalid',
    )
  }
  const invalid = structuredClone(captions)
  Object.assign(invalid.files[2], { caption: 'é'.repeat(64 * 1024) })
  vi.mocked(invoke).mockResolvedValue({ value: invalid })
  await expect(nativeIncomingShare().call('claim')).rejects.toThrow(
    'Incoming share: invalid',
  )
})
