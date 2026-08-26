import { expect, test } from '@playwright/test'
import { signIn } from './helpers'

const QR_BINARY_BASE64 = Buffer.from([0, 1, 2, 3, 127, 128, 254, 255])
  .toString('base64')
  .replace(/=+$/, '')

async function openQrSetup(page: import('@playwright/test').Page) {
  await signIn(page)
  await page.goto('/accounts')
  await page.getByRole('tab', { name: 'Sign in and verify with QR' }).click()
  await page.getByLabel('Expected Matrix user ID').fill('@me:hs')
}

test('a rendered binary QR image scans back to the exact original bytes', async ({
  page,
}) => {
  await openQrSetup(page)
  await page.getByRole('button', { name: 'Start QR sign-in' }).click()
  const canvas = page.getByRole('img', { name: 'Matrix sign-in QR code' })
  await expect(canvas).toBeVisible()
  const image = await canvas.screenshot()

  await page.getByRole('button', { name: 'Cancel QR sign-in' }).click()
  await expect(page.getByText('QR sign-in cancelled.')).toBeVisible()
  await page.getByRole('button', { name: 'Start again' }).click()
  await page.getByLabel('Expected Matrix user ID').fill('@me:hs')
  await page.getByLabel('Scan a QR code with this device').check()
  await page.getByRole('button', { name: 'Start QR sign-in' }).click()
  await page.getByLabel('Choose QR image').setInputFiles({
    name: 'rendered-qr.png',
    mimeType: 'image/png',
    buffer: image,
  })

  await expect(page.getByLabel('Check code')).toHaveText('42')
  const result = await page.evaluate(() =>
    fetch('/__e2e/qr-submission').then((response) => response.json()),
  )
  expect(result.data.expected).toBe(QR_BINARY_BASE64)
  expect(result.data.submitted).toBe(QR_BINARY_BASE64)
})

test('camera denial keeps image upload available', async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'mediaDevices', {
      configurable: true,
      value: {
        getUserMedia: () =>
          Promise.reject(
            new DOMException('permission denied', 'NotAllowedError'),
          ),
      },
    })
  })
  await openQrSetup(page)
  await page.getByLabel('Scan a QR code with this device').check()
  await page.getByRole('button', { name: 'Start QR sign-in' }).click()
  await page.getByRole('button', { name: 'Start camera' }).click()

  await expect(
    page.getByText(/permission denied.*Choose an image instead/i),
  ).toBeVisible()
  await expect(page.getByLabel('Choose QR image')).toBeEnabled()
})

test('multiple cameras can be selected after permission is granted', async ({
  page,
}) => {
  await page.addInitScript(() => {
    const requests: string[] = []
    Object.defineProperty(window, '__axonCameraRequests', {
      configurable: true,
      value: requests,
    })
    const mediaDevices = {
      getUserMedia: (constraints: MediaStreamConstraints) => {
        const video = constraints.video as MediaTrackConstraints
        const requested = video.deviceId as
          { exact?: string } | string | undefined
        const deviceId =
          typeof requested === 'object' && requested.exact !== undefined
            ? requested.exact
            : 'rear'
        requests.push(deviceId)
        const track = {
          stop: () => {},
          getSettings: () => ({ deviceId }),
        }
        return Promise.resolve({
          getTracks: () => [track],
          getVideoTracks: () => [track],
        } as unknown as MediaStream)
      },
      enumerateDevices: () =>
        Promise.resolve([
          {
            deviceId: 'rear',
            groupId: 'built-in',
            kind: 'videoinput' as const,
            label: 'Built-in rear camera',
            toJSON: () => ({}),
          },
          {
            deviceId: 'usb',
            groupId: 'external',
            kind: 'videoinput' as const,
            label: 'USB document camera',
            toJSON: () => ({}),
          },
        ]),
      addEventListener: () => {},
      removeEventListener: () => {},
    }
    Object.defineProperty(navigator, 'mediaDevices', {
      configurable: true,
      value: mediaDevices,
    })
    const streams = new WeakMap<HTMLMediaElement, MediaStream | null>()
    Object.defineProperty(HTMLMediaElement.prototype, 'srcObject', {
      configurable: true,
      get() {
        return streams.get(this) ?? null
      },
      set(value: MediaStream | null) {
        streams.set(this, value)
      },
    })
    Object.defineProperty(HTMLMediaElement.prototype, 'play', {
      configurable: true,
      value: () => Promise.resolve(),
    })
    Object.defineProperty(HTMLMediaElement.prototype, 'pause', {
      configurable: true,
      value: () => {},
    })
  })
  await openQrSetup(page)
  await page.getByLabel('Scan a QR code with this device').check()
  await page.getByRole('button', { name: 'Start QR sign-in' }).click()
  await page.getByRole('button', { name: 'Start camera' }).click()

  const picker = page.getByLabel('Camera', { exact: true })
  await expect(picker).toHaveValue('rear')
  await expect(picker.locator('option')).toHaveText([
    'Built-in rear camera',
    'USB document camera',
  ])

  await picker.selectOption('usb')
  await expect(picker).toHaveValue('usb')
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (window as unknown as { __axonCameraRequests: string[] })
            .__axonCameraRequests,
      ),
    )
    .toEqual(['rear', 'usb'])
})

test('Accounts acquisition stays usable at desktop and phone widths', async ({
  page,
}) => {
  await signIn(page)
  for (const viewport of [
    { width: 1280, height: 800 },
    { width: 390, height: 844 },
  ]) {
    await page.setViewportSize(viewport)
    await page.goto('/accounts')
    await expect(page.getByRole('heading', { name: 'Accounts' })).toBeVisible()
    await expect(
      page.getByRole('tab', { name: 'Sign in with password' }),
    ).toBeVisible()
    await expect(
      page.getByRole('tab', { name: 'Sign in and verify with QR' }),
    ).toBeVisible()
    const box = await page.locator('.account-add').boundingBox()
    expect(box).not.toBeNull()
    expect(box!.x).toBeGreaterThanOrEqual(0)
    expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width)
  }
})
