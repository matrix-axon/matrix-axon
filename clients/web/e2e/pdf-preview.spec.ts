import { expect, test, type Page } from '@playwright/test'
import { readFileSync } from 'node:fs'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

// Use the shipping policy, so this exercises the restriction that a plain
// browser build lacks. No shell IPC is needed: media still arrives through the
// real media service and becomes a refcounted blob URL before PDF.js sees it.
const { app } = JSON.parse(
  readFileSync(
    new URL('../src-tauri/tauri.conf.json', import.meta.url),
    'utf8',
  ),
) as { app: { security: { csp: string } } }

// A self-contained two-page PDF. Solid red/blue pages let the test prove pixels
// were drawn, rather than just counting the canvases created before rendering.
function pdfFixture(): Buffer {
  const objects = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Resources << >> /Contents 4 0 R >>',
    '',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Resources << >> /Contents 6 0 R >>',
    '',
  ]
  for (const [index, color] of [
    [3, '1 0 0'],
    [5, '0 0 1'],
  ] as const) {
    const stream = `${color} rg 0 0 200 300 re f\n`
    objects[index] =
      `<< /Length ${stream.length} >>\nstream\n${stream}endstream`
  }
  let pdf = '%PDF-1.4\n'
  const offsets = [0]
  for (const [index, object] of objects.entries()) {
    offsets.push(pdf.length)
    pdf += `${index + 1} 0 obj\n${object}\nendobj\n`
  }
  const xref = pdf.length
  pdf += `xref\n0 ${offsets.length}\n0000000000 65535 f \n`
  for (const offset of offsets.slice(1)) {
    pdf += `${String(offset).padStart(10, '0')} 00000 n \n`
  }
  pdf += `trailer\n<< /Size ${offsets.length} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`
  return Buffer.from(pdf)
}

async function openPdf(page: Page, body: Buffer) {
  await page.route('**/*', async (route) => {
    if (route.request().resourceType() !== 'document') return route.fallback()
    const response = await route.fetch()
    await route.fulfill({
      response,
      headers: {
        ...response.headers(),
        'content-security-policy': app.security.csp,
      },
    })
  })
  await page.route(/\/rooms\/[^/]+\/timeline(\?|$)/, (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            {
              account_id: ACCOUNT_ID,
              room_id: ROOM_ID,
              event_id: '$pdf-preview',
              sender: '@alice:hs',
              origin_ts: Date.now(),
              arrival_order: 1,
              type: 'm.room.message',
              state_key: null,
              body: 'pages.pdf',
              content: {
                msgtype: 'm.file',
                body: 'pages.pdf',
                url: 'mxc://hs/pdf-preview',
                info: { mimetype: 'application/pdf', size: body.length },
              },
              redacted: false,
              edited: false,
              edit_count: 0,
              relates_to: null,
              reactions: null,
            },
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.route(/\/v1\/media\/[^/]+\/hs\/pdf-preview$/, (route) => {
    expect(route.request().headers().authorization).toBe('Bearer e2e-token')
    // The media proxy deliberately downgrades PDFs to octet-stream.
    return route.fulfill({ contentType: 'application/octet-stream', body })
  })
  await signIn(page)
  await page.goto(ROOM_URL)
  await page
    .getByRole('button', { name: 'Open pages.pdf', exact: true })
    .click()
  await expect(page.getByRole('dialog')).toBeVisible()
}

test('PDF preview paints every page under the packaged shell CSP', async ({
  page,
}) => {
  await openPdf(page, pdfFixture())
  await expect(page.locator('.pdf-page-count')).toHaveText('1 / 2')
  for (const [number, color] of [
    [1, [255, 0, 0, 255]],
    [2, [0, 0, 255, 255]],
  ] as const) {
    const holder = page.locator(`.pdf-page[data-page="${number}"]`)
    await holder.scrollIntoViewIfNeeded()
    await expect
      .poll(() =>
        holder.locator('canvas').evaluateAll((canvases) => {
          const canvas = canvases[0] as HTMLCanvasElement | undefined
          return canvas
            ? [
                ...canvas
                  .getContext('2d')!
                  .getImageData(canvas.width / 2, canvas.height / 2, 1, 1).data,
              ]
            : []
        }),
      )
      .toEqual(color)
  }
  await page.getByRole('button', { name: 'Close', exact: true }).click()
  await page
    .getByRole('button', { name: 'Open pages.pdf', exact: true })
    .click()
  await expect(page.locator('.pdf-page-count')).toHaveText('1 / 2')
})

test('an invalid PDF still offers Download under the packaged shell CSP', async ({
  page,
}) => {
  await openPdf(page, Buffer.from('<html>This is not a PDF.</html>'))
  await expect(
    page.getByText(
      'Could not render this PDF — use Download to open it in another app.',
    ),
  ).toBeVisible()
  await expect(
    page
      .getByRole('dialog')
      .getByRole('button', { name: 'Download pages.pdf', exact: true }),
  ).toBeVisible()
})
