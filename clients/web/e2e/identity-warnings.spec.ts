import { expect, test, type Locator, type Page } from '@playwright/test'
import { ACCOUNT_ID, ROOM_URL, expectRoomReady, signIn } from './helpers'

async function expectWithinViewport(page: Page, target: Locator) {
  await expect(target).toBeVisible()
  const viewport = page.viewportSize()!
  const box = (await target.boundingBox())!
  expect(box.x).toBeGreaterThanOrEqual(0)
  expect(box.y).toBeGreaterThanOrEqual(0)
  expect(box.x + box.width).toBeLessThanOrEqual(viewport.width)
  expect(box.y + box.height).toBeLessThanOrEqual(viewport.height)
}

test('identity warnings leave room sending available and the review dialog fits', async ({
  page,
}) => {
  let warnings = [
    {
      user_id: `@${'long-matrix-id'.repeat(7)}:hs`,
      kind: 'pin_violation',
      fingerprint: 'a'.repeat(64),
    },
    ...Array.from({ length: 8 }, (_, index) => ({
      user_id: `@member-${index}:hs`,
      kind: 'verification_violation',
      fingerprint: 'b'.repeat(64),
    })),
  ]
  await page.route('**/v1/accounts/*/rooms/*/info', (route) =>
    route.fulfill({ json: { data: { identity_changes: warnings } } }),
  )
  await page.route(
    '**/v1/accounts/*/users/*/identity/withdraw_verification',
    (route) => {
      const userId = decodeURIComponent(
        new URL(route.request().url()).pathname.split('/').at(-3)!,
      )
      warnings = warnings.filter((warning) => warning.user_id !== userId)
      return route.fulfill({ status: 204 })
    },
  )
  // Keep the mock server's shared timeline unchanged for later specs.
  await page.route('**/v1/accounts/*/rooms/*/send', (route) =>
    route.fulfill({ json: { data: { event_id: '$warning-test' } } }),
  )

  await signIn(page)
  await page.goto(ROOM_URL)
  await expectRoomReady(page)
  const review = page.getByRole('button', { name: 'Review identity changes' })
  await expect(review).toHaveCount(2)
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await expectWithinViewport(page, composer)
  await composer.fill('Sending with a pending identity warning')
  const sent = page.waitForRequest(
    (request) => request.method() === 'POST' && request.url().endsWith('/send'),
  )
  await page.getByRole('button', { name: 'Send', exact: true }).click()
  expect((await sent).postDataJSON().body).toBe(
    'Sending with a pending identity warning',
  )

  await review.last().click()
  const dialog = page.getByRole('dialog', { name: 'Review identity changes' })
  await expectWithinViewport(page, dialog.locator('.overlay-panel'))
  await expect(dialog.getByText(warnings[0].user_id)).toBeVisible()
  await expect(dialog.getByRole('button', { name: 'Close' })).toBeFocused()
  const lastAction = dialog.getByRole('button', {
    name: 'Withdraw verification for @member-7:hs',
  })
  await lastAction.scrollIntoViewIfNeeded()
  await expectWithinViewport(page, lastAction)
  await lastAction.press('Enter')
  await expect(lastAction).toHaveCount(0)
  await expect(
    dialog.getByRole('button', {
      name: 'Withdraw verification for @member-6:hs',
    }),
  ).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  await expect(review.last()).toBeFocused()
})

test('a failed acknowledgment keeps focus and ignores duplicate activation', async ({
  page,
}) => {
  let fingerprint = 'a'.repeat(64)
  let attempts = 0
  const pending = Promise.withResolvers<void>()
  await page.route('**/v1/accounts/*/rooms/*/info', (route) =>
    route.fulfill({
      json: {
        data: {
          identity_changes: [
            { user_id: '@member:hs', kind: 'pin_violation', fingerprint },
          ],
        },
      },
    }),
  )
  await page.route(
    '**/v1/accounts/*/users/*/identity/accept',
    async (route) => {
      attempts += 1
      await pending.promise
      fingerprint = 'b'.repeat(64)
      await route.fulfill({
        status: 409,
        json: {
          error: {
            code: 'conflict',
            message: 'The identity warning changed. Review it again.',
          },
        },
      })
    },
  )

  await signIn(page)
  await page.goto(ROOM_URL)
  await expectRoomReady(page)
  await page
    .getByRole('button', { name: 'Review identity changes' })
    .last()
    .click()
  const dialog = page.getByRole('dialog', { name: 'Review identity changes' })
  const action = dialog.getByRole('button', {
    name: 'Accept new identity for @member:hs',
  })
  const click = await action.elementHandle()
  try {
    await action.press('Enter')
    await expect.poll(() => attempts).toBe(1)
    await expect(action).toBeDisabled()
    await expect(action).toBeFocused()
    await page.keyboard.press('Enter')
    const refreshed = page.waitForResponse(
      (response) =>
        response.request().method() === 'GET' &&
        response.url().endsWith('/info'),
    )
    pending.resolve()
    await expect(dialog.getByRole('alert')).toContainText(
      'The identity warning changed',
    )
    const response = await refreshed
    expect((await response.json()).data.identity_changes[0].fingerprint).toBe(
      fingerprint,
    )
    await expect(action).toBeEnabled()
    await expect(action).toBeFocused()
    expect(
      await click?.evaluate((button) => button === document.activeElement),
    ).toBe(true)
    expect(attempts).toBe(1)
  } finally {
    pending.resolve()
  }
})

test('own-identity recovery opens in place and keeps the room draft', async ({
  page,
}) => {
  await page.route('**/v1/accounts', async (route) => {
    const response = await route.fetch()
    const body = await response.json()
    for (const account of body.data) {
      if (account.account_id === ACCOUNT_ID) {
        account.own_identity_changed = true
        account.verified = false
      }
    }
    await route.fulfill({ response, json: body })
  })

  await signIn(page)
  await page.goto(ROOM_URL)
  await expectRoomReady(page)
  const warning = page.locator('.trust-warning').filter({
    hasText: 'Your Matrix identity changed.',
  })
  await expect(
    warning.getByRole('button', { name: 'Verify this device' }),
  ).toBeVisible()
  const draft = 'Draft before opening account recovery'
  await page.getByRole('textbox', { name: /^Message/ }).fill(draft)
  await page.evaluate(() => {
    document.documentElement.dataset.identityReviewTest = 'same-document'
  })
  await warning.getByRole('link', { name: 'Recover keys' }).click()
  await expect(page).toHaveURL(new RegExp(`/accounts\\?recover=${ACCOUNT_ID}$`))
  const recoveryKey = page.getByLabel('Recovery key', { exact: true })
  await expect(recoveryKey).toHaveCount(1)
  await expect(recoveryKey).toBeFocused()
  await expect(page.locator('html')).toHaveAttribute(
    'data-identity-review-test',
    'same-document',
  )

  await page.evaluate(() => history.back())
  await expect(page.getByRole('textbox', { name: /^Message/ })).toHaveValue(
    draft,
  )
  await expect(page.locator('html')).toHaveAttribute(
    'data-identity-review-test',
    'same-document',
  )
})
