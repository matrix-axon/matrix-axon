import { spawn } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import type { FullResult, Suite } from '@playwright/test/reporter'
import { afterAll, describe, expect, it } from 'vitest'
import FlakyPolicyReporter from './flaky-policy-reporter'

/**
 * `flaky-policy-reporter.ts` is the only thing that fails a CI run on a flaky
 * test (#391), and it fails *open* if Playwright stops honouring it. So this
 * drives the real runner on `policy-canary/` and reads the exit code. A change
 * in `outcome()`, in retries, or in how `onEnd`'s returned status is applied
 * shows up here as a wrong exit code, not as a quietly green lane.
 */

// vitest's root is clients/web. Paths come from it rather than from
// `import.meta.url`, which the jsdom environment does not give as a file URL.
const here = join(process.cwd(), 'e2e')
const cli = createRequire(join(here, 'noop.js')).resolve('@playwright/test/cli')
const config = join(here, 'policy-canary', 'playwright.config.ts')
const scratch = mkdtempSync(join(process.env.TMPDIR ?? tmpdir(), 'policy-'))

afterAll(() => rmSync(scratch, { recursive: true, force: true }))

interface Run {
  code: number | null
  output: string
  summary: string
}

function run(project: string, scenario: string): Promise<Run> {
  const summary = join(
    scratch,
    `${project}-${scenario.replace(/\W+/g, '-')}.md`,
  )
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [
        cli,
        'test',
        '-c',
        config,
        `--project=${project}`,
        // The grep sees the whole title path, so anchor the last segment only.
        '-g',
        `(^| )${scenario}$`,
        // Outside the tree: a test-results/ here would not be ignored.
        `--output=${summary}.out`,
      ],
      {
        cwd: join(here, 'policy-canary'),
        env: {
          ...process.env,
          GITHUB_STEP_SUMMARY: summary,
          PLAYWRIGHT_HTML_OPEN: 'never',
        },
      },
    )
    let output = ''
    child.stdout.on('data', (chunk) => (output += chunk))
    child.stderr.on('data', (chunk) => (output += chunk))
    child.on('error', reject)
    child.on('close', (code) => {
      let written = ''
      try {
        written = readFileSync(summary, 'utf8')
      } catch {
        // No summary is written when nothing was tolerated.
      }
      resolve({ code, output, summary: written })
    })
  })
}

describe('flaky-policy reporter, through the real runner', () => {
  // Each case also checks the runner's own count, so a scenario that silently
  // stopped running (exit 1 from "no tests found") cannot pass for a failure.
  const cases: Array<{
    name: string
    project: string
    scenario: string
    counted: RegExp
    code: number
    tolerated: boolean
  }> = [
    {
      name: 'passes a clean run',
      project: 'webkit-desktop',
      scenario: 'passes',
      counted: /\b1 passed\b/,
      code: 0,
      tolerated: false,
    },
    {
      name: 'fails an ordinary flake, as --fail-on-flaky-tests did',
      project: 'webkit-desktop',
      scenario: 'ordinary flake',
      counted: /\b1 flaky\b/,
      code: 1,
      tolerated: false,
    },
    {
      name: 'accepts a WebKit driver-error flake, and says so',
      project: 'webkit-desktop',
      scenario: 'driver flake',
      counted: /\b1 flaky\b/,
      code: 0,
      tolerated: true,
    },
    {
      name: 'accepts a WebKit network-process crash flake',
      project: 'webkit-desktop',
      scenario: 'crash flake',
      counted: /\b1 flaky\b/,
      code: 0,
      tolerated: true,
    },
    {
      name: 'fails the same signature on another engine',
      project: 'chromium',
      scenario: 'driver flake',
      counted: /\b1 flaky\b/,
      code: 1,
      tolerated: false,
    },
    {
      name: 'fails a hard failure whatever its error says',
      project: 'webkit-desktop',
      scenario: 'hard failure',
      counted: /\b1 failed\b/,
      code: 1,
      tolerated: false,
    },
  ]

  // Concurrent: each case is a separate runner process of about a second.
  for (const c of cases) {
    it.concurrent(c.name, { timeout: 60_000 }, async () => {
      const { code, output, summary } = await run(c.project, c.scenario)
      expect(output).toMatch(c.counted)
      expect(code).toBe(c.code)
      if (c.tolerated) {
        expect(output).toContain('::warning title=WebKit infrastructure flake')
        expect(summary).toContain(c.scenario)
      } else {
        expect(summary).toBe('')
      }
    })
  }
})

describe('flaky-policy reporter, failing closed', () => {
  const passed = { status: 'passed' } as FullResult

  it('fails the run when it never received the suite', async () => {
    expect(await new FlakyPolicyReporter().onEnd(passed)).toEqual({
      status: 'failed',
    })
  })

  it('fails the run when reading the suite throws', async () => {
    const reporter = new FlakyPolicyReporter()
    reporter.onBegin(
      {} as never,
      {
        allTests: () => {
          throw new Error('the reporter API changed')
        },
      } as unknown as Suite,
    )
    expect(await reporter.onEnd(passed)).toEqual({ status: 'failed' })
  })
})
