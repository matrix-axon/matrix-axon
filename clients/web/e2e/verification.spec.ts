import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, signIn } from './helpers'

const SIBLING = {
  device_id: 'ELEMENT',
  display_name: 'Element',
  algorithms: ['m.megolm.v1.aes-sha2'],
  is_verified: true,
  is_cross_signed_by_owner: true,
  local_trust_state: 'verified',
}
const OWN = {
  device_id: 'DEVICE',
  display_name: 'Axon',
  algorithms: ['m.megolm.v1.aes-sha2'],
  is_verified: true,
  is_cross_signed_by_owner: true,
  local_trust_state: 'verified',
}

test.describe.configure({ mode: 'serial' })

test.beforeEach(async ({ request }) => {
  expect((await request.post('/__e2e/verify-reset')).ok()).toBe(true)
})
test.afterEach(async ({ request }) => {
  await request.post('/__e2e/verify-reset')
})

test('outbound SAS from Accounts: pick a sibling, compare emoji, complete', async ({
  page,
  request,
}) => {
  await request.post('/__e2e/verify-devices', {
    data: {
      account_id: ACCOUNT_ID,
      user_id: '@me:hs',
      devices: [OWN, SIBLING],
    },
  })
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/accounts')
  await expect(
    page.getByRole('button', { name: 'Verify this device' }).first(),
  ).toBeVisible()
  await page.getByRole('button', { name: 'Verify this device' }).first().click()
  await page.getByRole('button', { name: /Element/ }).click()
  const dialog = page.getByRole('dialog', { name: /Verifying/ })
  await expect(dialog).toBeVisible()
  const posted = await page.evaluate(() =>
    fetch('/v1/accounts/11111111-1111-4111-8111-111111111111/verify')
      .then((r) => r.json())
      .then((body) => body.data[0]?.flow_id as string),
  )
  await request.post('/__e2e/push-verification', {
    data: {
      account_id: ACCOUNT_ID,
      flow_id: posted,
      kind: 'sas',
      device_id: 'ELEMENT',
    },
  })
  await expect(dialog.getByText('Dog')).toBeVisible()
  await dialog.getByRole('button', { name: 'They match' }).click()
  await request.post('/__e2e/push-verification', {
    data: {
      account_id: ACCOUNT_ID,
      flow_id: posted,
      kind: 'done',
      device_id: 'ELEMENT',
    },
  })
  await expect(dialog.getByText('Verification complete.')).toBeVisible()
  await dialog.getByRole('button', { name: 'OK' }).click()
  await expect(dialog).toHaveCount(0)
})

test('Escape parks a live flow on /accounts and the chip restores it', async ({
  page,
  request,
}) => {
  await request.post('/__e2e/verify-devices', {
    data: {
      account_id: ACCOUNT_ID,
      user_id: '@me:hs',
      devices: [OWN, SIBLING],
    },
  })
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/accounts')
  await page.getByRole('button', { name: 'Verify this device' }).first().click()
  await page.getByRole('button', { name: /Element/ }).click()
  const dialog = page.getByRole('dialog', { name: /Verifying/ })
  await expect(dialog).toBeVisible()
  // The Escape binding registers in the same effect flush that pulls focus
  // into the dialog, and Preact runs effects after paint. Waiting on the
  // dialog alone leaves a frame where a keypress lands on nothing and is gone
  // — keys, unlike clicks, are not retried.
  await expect(dialog.getByRole('button', { name: 'Close' })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  await expect(
    page.getByRole('button', { name: /Device verification/ }),
  ).toBeVisible()
  await page.getByRole('button', { name: /Device verification/ }).click()
  await expect(page.getByRole('dialog', { name: /Verifying/ })).toBeVisible()
})

test('inbound request appears as a chip and Compare opens the modal', async ({
  page,
  request,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await request.post('/__e2e/push-verification', {
    data: {
      account_id: ACCOUNT_ID,
      flow_id: 'flow-inbound',
      kind: 'requested',
      device_id: 'PHONE',
    },
  })
  await expect(
    page.getByRole('button', { name: /Device verification/ }),
  ).toBeVisible()
  await page.getByRole('button', { name: 'Verify PHONE' }).click()
  await expect(page.getByRole('dialog')).toBeVisible()
})

test('Decline on an inbound row removes it without opening the modal', async ({
  page,
  request,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await request.post('/__e2e/push-verification', {
    data: {
      account_id: ACCOUNT_ID,
      flow_id: 'flow-decline',
      kind: 'requested',
      device_id: 'PHONE',
    },
  })
  await expect(page.getByRole('button', { name: 'Decline' })).toBeVisible()
  await page.getByRole('button', { name: 'Decline' }).click()
  await expect(page.getByRole('button', { name: 'Verify PHONE' })).toHaveCount(
    0,
  )
  await expect(page.getByRole('dialog')).toHaveCount(0)
})
