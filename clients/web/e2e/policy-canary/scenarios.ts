import { test } from '@playwright/test'
import { CRASH_SIGNATURE } from '../webkit-infra'

/**
 * One test per outcome the flaky-policy reporter has to tell apart, selected
 * with `-g`. Named `scenarios.ts`, not `*.spec.ts`, so the e2e lanes never
 * collect it; only `playwright.config.ts` in this directory does.
 */

test('passes', () => {})

test('ordinary flake', () => {
  if (test.info().retry === 0) {
    throw new Error('an ordinary race in our own code')
  }
})

test('driver flake', () => {
  if (test.info().retry === 0) {
    throw new Error('page.goto: WebKit encountered an internal error')
  }
})

test('crash flake', () => {
  if (test.info().retry === 0) {
    throw new Error(`${CRASH_SIGNATURE} (console: Network process crashed.)`)
  }
})

test('hard failure', () => {
  throw new Error('page.goto: WebKit encountered an internal error')
})
