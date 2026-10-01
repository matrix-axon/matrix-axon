import { appendFileSync } from 'node:fs'
import type {
  FullConfig,
  FullResult,
  Reporter,
  Suite,
  TestCase,
} from '@playwright/test/reporter'
import { WEBKIT_INFRA_FAILURE } from './webkit-infra'

/**
 * The CI lanes' flaky-test policy, in place of `--fail-on-flaky-tests` (#391).
 *
 * A test that fails and then passes on its retry still fails the job, with one
 * exception: every failed attempt ran on WebKit and carries the
 * `WEBKIT_INFRA_FAILURE` signature, i.e. the browser broke rather than the
 * app or the spec. Those are accepted, and listed as warnings and in the job
 * summary so a rising rate stays visible. A test that fails every attempt
 * fails the job whatever its error said; this only decides what a *passing*
 * retry is worth.
 *
 * It fails closed. It is the only thing between a flaky test and a green run,
 * so anything unexpected inside it, whether an exception or a suite it never
 * received, fails the run rather than letting it through. A reporter that fails
 * to load already fails the run before any test starts. What it cannot catch
 * itself, a Playwright change in `outcome()` or in how `onEnd`'s status is
 * honoured, is pinned by `flaky-policy-reporter.vitest.ts`, which drives the
 * real runner.
 */
export default class FlakyPolicyReporter implements Reporter {
  private suite: Suite | undefined

  onBegin(_config: FullConfig, suite: Suite): void {
    this.suite = suite
  }

  // Async because the reporter contract only accepts a status override as a
  // promise; a synchronous return is typed `void`.
  async onEnd(
    result: FullResult,
  ): Promise<{ status: FullResult['status'] } | undefined> {
    try {
      return this.decide(result)
    } catch (error) {
      console.error('flaky-policy reporter failed; failing the run', error)
      return { status: 'failed' }
    }
  }

  private decide(
    result: FullResult,
  ): { status: FullResult['status'] } | undefined {
    if (this.suite === undefined) {
      console.error(
        'flaky-policy reporter never saw the suite; failing the run',
      )
      return { status: 'failed' }
    }
    const flaky = this.suite
      .allTests()
      .filter((test) => test.outcome() === 'flaky')
    const tolerated = flaky.filter(isWebKitInfraFlake)
    const blocking = flaky.filter((test) => !tolerated.includes(test))

    for (const test of tolerated) {
      console.log(
        `::warning title=WebKit infrastructure flake (tolerated)::${title(test)}`,
      )
    }
    if (tolerated.length > 0 && process.env.GITHUB_STEP_SUMMARY) {
      appendFileSync(
        process.env.GITHUB_STEP_SUMMARY,
        [
          '### WebKit infrastructure flakes (tolerated, #391)',
          '',
          ...tolerated.map((test) => `- ${title(test)}`),
          '',
        ].join('\n'),
      )
    }
    for (const test of blocking) {
      console.log(`flaky, failing the run: ${title(test)}`)
    }

    if (blocking.length > 0 && result.status === 'passed') {
      return { status: 'failed' }
    }
    return undefined
  }
}

function isWebKitInfraFlake(test: TestCase): boolean {
  const project = test.parent.project()
  const browser = project?.use.defaultBrowserType ?? project?.use.browserName
  if (browser !== 'webkit') {
    return false
  }
  const failed = test.results.filter(
    (result) => result.status !== 'passed' && result.status !== 'skipped',
  )
  return (
    failed.length > 0 &&
    failed.every((result) =>
      result.errors.some((error) =>
        WEBKIT_INFRA_FAILURE.test(
          `${error.message ?? ''}\n${error.value ?? ''}`,
        ),
      ),
    )
  )
}

function title(test: TestCase): string {
  return `[${test.parent.project()?.name}] ${test.titlePath().filter(Boolean).slice(1).join(' › ')}`
}
