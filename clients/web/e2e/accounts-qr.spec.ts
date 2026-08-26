import { expect, test } from '@playwright/test'
import { signIn } from './helpers'

const QR_BINARY_BASE64 = Buffer.from([0, 1, 2, 3, 127, 128, 254, 255])
  .toString('base64')
  .replace(/=+$/, '')

async function openQrSetup(page: import('@playwright/test').Page) {
  await signIn(page)
  await page.goto('/accounts')
  await page.getByRole('tab', { name: 'Sign in with QR code' }).click()
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

test('the two-digit check code uses one input rendered as two cells', async ({
  page,
}) => {
  await page.route('**/v1/accounts/login/qr', async (route) => {
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        data: {
          flow_id: '10000000-0000-4000-8000-000000000001',
          expected_user_id: '@me:hs',
          presentation: 'display',
          stage: 'check_code_required',
        },
      }),
    })
  })
  await openQrSetup(page)
  await page.getByRole('button', { name: 'Start QR sign-in' }).click()

  const input = page.getByLabel('Two-digit check code')
  const cells = page.locator('.segmented-code-cell')
  await expect(input).toHaveCount(1)
  await expect(cells).toHaveCount(2)
  await input.fill('4')
  await expect(cells.nth(0)).toHaveText('4')
  await expect(cells.nth(1)).toHaveText('')
  await input.fill('42')
  await expect(cells.nth(0)).toHaveText('4')
  await expect(cells.nth(1)).toHaveText('2')
  await expect(page.getByRole('button', { name: 'Confirm code' })).toBeEnabled()
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

test('a deactivated account can sign in again with its stored identity', async ({
  page,
}) => {
  let active = false
  let loginBody: unknown
  const account = {
    account_id: '33333333-3333-4333-8333-333333333333',
    user_id: '@returning:hs',
    homeserver_url: 'https://matrix.hs',
    state: 'deactivated',
    sync_state: 'connecting',
    verified: false,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  await page.route('**/v1/accounts', async (route) => {
    if (route.request().method() !== 'GET') {
      await route.continue()
      return
    }
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        data: [{ ...account, state: active ? 'active' : 'deactivated' }],
      }),
    })
  })
  await page.route('**/v1/accounts/login', async (route) => {
    loginBody = route.request().postDataJSON()
    active = true
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ data: { ...account, state: 'active' } }),
    })
  })

  await signIn(page)
  await page.goto('/settings')
  await expect(page).toHaveURL(/\/accounts$/)
  await expect(page.getByText('deactivated')).toBeVisible()
  await expect(page.getByText('connecting')).toHaveCount(0)

  await page.getByRole('button', { name: 'Sign in again' }).click()
  const userId = page.getByLabel('Matrix user ID')
  await expect(userId).toHaveValue('@returning:hs')
  await expect(userId).toHaveAttribute('readonly', '')
  await expect(page.getByLabel('Password')).toBeFocused()
  await page.getByLabel('Password').fill('test-password')
  await page.getByRole('button', { name: 'Reactivate account' }).click()

  await expect
    .poll(() => loginBody)
    .toEqual({
      username: '@returning:hs',
      password: 'test-password',
      homeserver_url: null,
    })
  await expect(page).toHaveURL(/\/$/)
})

test('QR stays selected when chosen immediately after logout and reactivation', async ({
  page,
}) => {
  let active = true
  const account = {
    account_id: '44444444-4444-4444-8444-444444444444',
    user_id: '@returning:hs',
    homeserver_url: 'https://matrix.hs',
    state: 'active',
    sync_state: 'ready',
    verified: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  await page.route('**/v1/accounts', async (route) => {
    if (route.request().method() !== 'GET') {
      await route.continue()
      return
    }
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        data: [{ ...account, state: active ? 'active' : 'deactivated' }],
      }),
    })
  })
  await page.route(`**/v1/accounts/${account.account_id}/logout`, (route) => {
    active = false
    return route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ data: { ...account, state: 'deactivated' } }),
    })
  })

  await signIn(page)
  await page.goto('/accounts')
  await page.getByRole('button', { name: 'Log out' }).click()

  await page
    .getByRole('button', { name: 'Sign in again' })
    .evaluate((button) => {
      ;(button as HTMLButtonElement).click()
      queueMicrotask(() => {
        const qrTab = [...document.querySelectorAll('[role="tab"]')].find(
          (tab) => tab.textContent?.trim() === 'Sign in with QR code',
        )
        if (!(qrTab instanceof HTMLButtonElement)) {
          throw new Error('QR sign-in tab did not render')
        }
        qrTab.click()
      })
    })

  await expect(page.getByLabel('Expected Matrix user ID')).toHaveValue(
    '@returning:hs',
  )
  await expect(page.getByLabel('Password')).toHaveCount(0)
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
      page.getByRole('tab', { name: 'Sign in with QR code' }),
    ).toBeVisible()
    const box = await page.locator('.account-add').boundingBox()
    expect(box).not.toBeNull()
    expect(box!.x).toBeGreaterThanOrEqual(0)
    expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width)
  }
})
