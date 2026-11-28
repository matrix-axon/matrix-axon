import { expect, test } from '@playwright/test'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { crc32, deflateSync } from 'node:zlib'
import { openRoom } from './helpers'

/**
 * Sending media end to end (ADR 0065, M-W8.5). This lane exists to prove the one
 * thing the jsdom unit tests structurally cannot: that a real browser's `fetch`
 * puts the *file's bytes* on the wire when handed a `File` as the request body.
 * Under vitest a jsdom `File` is not undici's `Blob`, so the body never
 * survives the hop and the byte assertion is meaningless there.
 */

const PNG_BYTES = Buffer.from(
  '89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489',
  'hex',
)

/**
 * A valid PNG with large *intrinsic* dimensions but a tiny file size (solid
 * black compresses to almost nothing).
 *
 * The 1x1 fixture above cannot catch layout bugs: an unconstrained `<img>`
 * renders at its intrinsic size, and 1px never fills anything. A staged photo
 * from a phone is thousands of pixels wide, which is what actually filled the
 * viewport when the strip had no CSS.
 */
function largePng(width: number, height: number): Buffer {
  const chunk = (type: string, data: Buffer) => {
    const head = Buffer.concat([Buffer.from(type, 'ascii'), data])
    const len = Buffer.alloc(4)
    len.writeUInt32BE(data.length)
    const crc = Buffer.alloc(4)
    crc.writeUInt32BE(crc32(head))
    return Buffer.concat([len, head, crc])
  }
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width, 0)
  ihdr.writeUInt32BE(height, 4)
  ihdr[8] = 8 // bit depth
  ihdr[9] = 2 // truecolour
  // One filter byte per scanline, then RGB triples — all zero.
  const raw = Buffer.alloc((width * 3 + 1) * height)
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

interface StagedUpload {
  kind: string
  filename: string
  content_type: string | null
  size_bytes: number
  sha1: string
}

async function staged(page: import('@playwright/test').Page) {
  const res = await page.request.get('/__e2e/uploads')
  return ((await res.json()) as { data: StagedUpload[] }).data
}

test('attaches a file, uploads its real bytes, and sends it with a caption', async ({
  page,
}) => {
  await openRoom(page)
  // The mock server outlives one spec run, so uploads accumulate: count from
  // where this test starts rather than from zero.
  const before = (await staged(page)).length

  await page.getByLabel('Attach a file').setInputFiles({
    name: 'cat.png',
    mimeType: 'image/png',
    buffer: PNG_BYTES,
  })

  // Staged in the composer, not yet uploaded — the pause is what lets a caption
  // be typed.
  await expect(page.locator('.composer-attachment')).toContainText('cat.png')
  expect(await staged(page)).toHaveLength(before)

  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.fill('look at this')
  await expect(composer).toHaveValue('look at this')
  await page.getByRole('button', { name: 'Send', exact: true }).click()

  await expect
    .poll(async () => (await staged(page)).length)
    .toBeGreaterThan(before)
  const upload = (await staged(page)).at(-1)!

  // The bytes made it, intact — the assertion this whole lane exists for.
  expect(upload.size_bytes).toBe(PNG_BYTES.length)
  expect(upload.sha1).toBe(createHash('sha1').update(PNG_BYTES).digest('hex'))
  // `kind` and `Content-Type` agree, which is what the server's `kind=image`
  // rule requires; both come from the one `File.type`.
  expect(upload.kind).toBe('image')
  expect(upload.content_type).toBe('image/png')
  expect(upload.filename).toBe('cat.png')

  // The sent event lands in the timeline, rendered as an image from the proxy.
  // Key the caption by text rather than `.media-figure`.last(): seed images
  // have figures too, and a late echo would make last() the wrong row.
  await expect(page.locator('.composer-attachment')).toHaveCount(0)
  const sent = page.locator('.media-figure', { hasText: 'look at this' })
  await expect(sent.locator('figcaption')).toHaveText('look at this')
  await expect(sent.locator('img')).toHaveAttribute('src', /^blob:/)
})

test('shows the picked image immediately, before the upload finishes', async ({
  page,
}) => {
  await openRoom(page)

  // Hold the upload open so the optimistic echo is observable.
  let release = () => {}
  const held = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/media/uploads**', async (route) => {
    await held
    await route.continue()
  })

  await page.getByLabel('Attach a file').setInputFiles({
    name: 'cat.png',
    mimeType: 'image/png',
    buffer: PNG_BYTES,
  })
  await page.getByRole('button', { name: 'Send', exact: true }).click()

  // The echo renders the local file while the bytes are still in flight — no
  // proxy fetch has resolved, and none could have.
  const echo = page.locator('.media-preview')
  await expect(echo).toHaveAttribute('src', /^blob:/)
  await expect(page.getByText('Sending…')).toBeVisible()

  release()
  await expect(page.getByText('Sending…')).toHaveCount(0)
})

/**
 * Multi-image send (ADR 0081). The byte-level assertions matter more here than
 * anywhere: jsdom cannot carry a `File`'s bytes through `fetch`, so "did all
 * three files arrive, distinct and in order" is only answerable in a browser.
 */
test('sends several picked images in order, with distinct bytes', async ({
  page,
}) => {
  await openRoom(page)
  const before = (await staged(page)).length

  // Three files with *different* bytes, so order is provable by sha1 rather
  // than by trusting the filenames.
  const files = ['a', 'b', 'c'].map((tag) => ({
    name: `${tag}.png`,
    mimeType: 'image/png',
    buffer: Buffer.concat([PNG_BYTES, Buffer.from(tag)]),
  }))
  await page.getByLabel('Attach a file').setInputFiles(files)

  // Three chips in the strip, not the single-file chip.
  await expect(page.locator('.composer-attachment-item')).toHaveCount(3)
  await expect(page.locator('.composer-attachment')).toHaveCount(0)
  await expect(page.getByText(/captions the first attachment/)).toBeVisible()

  await page.getByRole('button', { name: 'Send', exact: true }).click()

  await expect
    .poll(async () => (await staged(page)).length, { timeout: 15_000 })
    .toBe(before + 3)

  // Arrived in the order picked, each with its own bytes.
  const uploads = (await staged(page)).slice(before)
  expect(uploads.map((u) => u.filename)).toEqual(['a.png', 'b.png', 'c.png'])
  expect(uploads.map((u) => u.sha1)).toEqual(
    files.map((f) => createHash('sha1').update(f.buffer).digest('hex')),
  )

  // The strip is cleared, and the three land as one gallery of their own —
  // the room already has a seeded gallery, so this is the *second*.
  await expect(page.locator('.composer-attachment-item')).toHaveCount(0)
  const sentGallery = page.locator('.gallery-row').last()
  await expect(sentGallery.locator('.gallery-cell')).toHaveCount(3)
})

test('keeps staged thumbnails small on a phone-sized viewport', async ({
  page,
}) => {
  // Reported from a device: a staged image is the *original*, thousands of
  // pixels wide, and an unconstrained `<img>` renders at that intrinsic size
  // and fills the screen.
  await openRoom(page)
  await page.setViewportSize({ width: 390, height: 844 })

  // 2400x1800, the shape of a phone camera file.
  const big = largePng(2400, 1800)
  await page.getByLabel('Attach a file').setInputFiles(
    ['a', 'b', 'c', 'd'].map((tag) => ({
      name: `${tag}.png`,
      mimeType: 'image/png',
      buffer: big,
    })),
  )
  await expect(page.locator('.composer-attachment-item')).toHaveCount(4)

  // Two layers hold this down, and the test has to prove both are load-bearing.
  //
  // The preview is downscaled from 2400px on staging, so `naturalWidth` here
  // is the smaller version — but it is still wider than the 390px viewport,
  // which means CSS is doing real work too. A fixture small enough to fit
  // would make the whole test vacuous, which is why the 1x1 PNG used
  // elsewhere is no good for this.
  const naturalWidth = () =>
    page
      .locator('.composer-attachment-thumb')
      .first()
      .evaluate((img) => (img as HTMLImageElement).naturalWidth)

  // Polled: the downscale is async, so the first paint may still be the
  // original. Waiting for it also makes this a genuine test of the downscale.
  await expect.poll(naturalWidth).toBeLessThan(2400)
  // Still wider than the viewport, so CSS is demonstrably load-bearing too.
  expect(await naturalWidth()).toBeGreaterThan(390)

  const sizes = await page
    .locator('.composer-attachment-item')
    .evaluateAll((items) =>
      items.map((el) => {
        const rect = el.getBoundingClientRect()
        return { w: rect.width, h: rect.height }
      }),
    )
  for (const size of sizes) {
    expect(size.w).toBeLessThan(100)
    expect(size.h).toBeLessThan(100)
  }

  // And the strip stays a strip: it must not push the composer off the
  // screen or eat the timeline.
  const stripHeight = await page
    .locator('.composer-attachment-strip')
    .evaluate((el) => el.getBoundingClientRect().height)
  expect(stripHeight).toBeLessThan(844 * 0.25)
})

test('removes one staged file without disturbing the rest', async ({
  page,
}) => {
  await openRoom(page)

  await page.getByLabel('Attach a file').setInputFiles(
    ['a', 'b', 'c'].map((tag) => ({
      name: `${tag}.png`,
      mimeType: 'image/png',
      buffer: PNG_BYTES,
    })),
  )
  await expect(page.locator('.composer-attachment-item')).toHaveCount(3)

  await page.getByLabel('Remove file 2 of 3, b.png').click()

  await expect(page.locator('.composer-attachment-item')).toHaveCount(2)
  // Renumbered, not merely shortened: the labels carry position, so the
  // survivors are now 1 and 2 of 2.
  await expect(page.getByLabel('Remove file 1 of 2, a.png')).toBeVisible()
  await expect(page.getByLabel('Remove file 2 of 2, c.png')).toBeVisible()
})

/**
 * A video's poster, dimensions and duration, made in a real browser (ADR 0142
 * § 7, issue 24). The unit tests stub `<video>` and the canvas, because jsdom
 * decodes neither; this is where the probe actually decodes a frame.
 *
 * The fixture is a 3 s, 320 × 240 VP8 WebM, which every Playwright engine
 * decodes. The mock server predates the new capabilities, so this test
 * answers `/v1/capabilities` as a current server does.
 */
test('sends a video as its own kind, with a poster, dimensions and duration', async ({
  page,
}) => {
  await page.route('**/v1/capabilities', (route) =>
    route.fulfill({
      json: {
        data: {
          edit_url_previews: true,
          media_upload_kinds: ['image', 'file', 'video', 'audio'],
          send_media_thumbnails: true,
        },
      },
    }),
  )
  const sends: Record<string, unknown>[] = []
  await page.route('**/send-media', async (route) => {
    sends.push(route.request().postDataJSON() as Record<string, unknown>)
    await route.continue()
  })
  await openRoom(page)
  const before = (await staged(page)).length

  const clip = readFileSync(new URL('./fixtures/clip.webm', import.meta.url))
  await page.getByLabel('Attach a file').setInputFiles({
    name: 'clip.webm',
    mimeType: 'video/webm',
    buffer: clip,
  })
  await page.getByRole('button', { name: 'Send', exact: true }).click()

  await expect.poll(() => sends.length).toBe(1)
  const uploads = (await staged(page)).slice(before)
  const original = uploads.find((u) => u.filename === 'clip.webm')
  const poster = uploads.find((u) => u.filename === 'poster.jpg')
  expect(original).toMatchObject({ kind: 'video', content_type: 'video/webm' })
  expect(original?.sha1).toBe(createHash('sha1').update(clip).digest('hex'))
  // Never upscaled, so the poster is the frame's own size; a JPEG of a
  // 320 × 240 test pattern is a few KiB.
  expect(poster).toMatchObject({ kind: 'image', content_type: 'image/jpeg' })
  expect(poster!.size_bytes).toBeGreaterThan(0)
  // A picture, not solid black: WebKit draws nothing from a video outside
  // the document, which JPEG encodes as black (the iPhone Safari poster).
  const posterBytes = await (
    await page.request.get('/__e2e/upload-bytes?filename=poster.jpg')
  ).body()
  const decoded = await page.evaluate(async (base64) => {
    const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0))
    const bitmap = await createImageBitmap(
      new Blob([bytes], { type: 'image/jpeg' }),
    )
    const canvas = document.createElement('canvas')
    canvas.width = bitmap.width
    canvas.height = bitmap.height
    const context = canvas.getContext('2d')!
    context.drawImage(bitmap, 0, 0)
    const data = context.getImageData(0, 0, bitmap.width, bitmap.height).data
    let min = 255
    let max = 0
    for (let i = 0; i < data.length; i += 4) {
      const luma = 0.299 * data[i] + 0.587 * data[i + 1] + 0.114 * data[i + 2]
      min = Math.min(min, luma)
      max = Math.max(max, luma)
    }
    return { width: bitmap.width, height: bitmap.height, spread: max - min }
  }, posterBytes.toString('base64'))
  expect(decoded).toMatchObject({ width: 320, height: 240 })
  expect(decoded.spread).toBeGreaterThan(64)
  expect(sends[0]).toMatchObject({
    upload_id: expect.any(String),
    info: { width: 320, height: 240, duration_ms: 3000 },
    txn_id: expect.stringMatching(/^[A-Za-z0-9._~-]{1,64}$/),
  })
  expect(typeof sends[0].thumbnail_upload_id).toBe('string')
  expect(sends[0].thumbnail_upload_id).not.toBe(sends[0].upload_id)
})
