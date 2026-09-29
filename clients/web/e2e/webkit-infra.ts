import type { ConsoleMessage, Page } from '@playwright/test'

/**
 * Failures that come from Playwright's WebKit itself rather than from the app
 * or the spec (#391). Two shapes have been seen in CI, and both recover when
 * the test is run again:
 *
 * - The driver's own navigation throws: `page.goto: WebKit encountered an
 *   internal error` (and the same from `page.reload`).
 * - The network process crashes under an app-initiated navigation. Nothing
 *   throws; the page logs `Network process crashed` and `WebKit encountered an
 *   internal error` to its console, and the navigation's `load` arrives too late
 *   for the wait on it. `withWebKitCrashSignature` puts that console evidence
 *   into the error, since otherwise it reads as an ordinary timeout.
 *
 * `flaky-policy-reporter.ts` matches a failed attempt against this, and only
 * this, before it accepts a passing retry.
 */
export const WEBKIT_INFRA_FAILURE =
  /WebKit encountered an internal error|WebKit network process crashed/

/** What the page's console says when WebKit's network process goes down. */
const CRASH_CONSOLE =
  /Network process crashed|WebKit encountered an internal error/

/**
 * Run `action`, and if it fails after the page has reported a WebKit
 * network-process crash, say so in the error. The failure is rethrown either
 * way: this names the cause, it does not retry or swallow anything.
 */
export async function withWebKitCrashSignature<T>(
  page: Page,
  action: () => Promise<T>,
): Promise<T> {
  const seen: string[] = []
  const onConsole = (message: ConsoleMessage) => {
    if (CRASH_CONSOLE.test(message.text())) {
      seen.push(message.text())
    }
  }
  page.on('console', onConsole)
  try {
    return await action()
  } catch (error) {
    if (seen.length === 0) {
      throw error
    }
    const cause = error instanceof Error ? error.message : String(error)
    throw new Error(
      `WebKit network process crashed during this navigation (console: ${seen[0]})\n${cause}`,
      { cause: error },
    )
  } finally {
    page.off('console', onConsole)
  }
}
