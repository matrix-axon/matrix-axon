import { execFileSync } from 'node:child_process'
import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

// What to keep from a failed simulator run, so that a failure on a runner can be
// read afterwards. The script that runs the tests puts its result bundles in a
// temporary directory and leaves them there, which a CI runner throws away.
// A host app that "crashed" says nothing more than that without them: the reason
// is in the crash report the host wrote, and in the result bundle.

/** The failures of one result bundle as JSON, or '' when Xcode cannot say. */
export const summarizeBundle = (bundle) => {
  try {
    return execFileSync(
      'xcrun',
      ['xcresulttool', 'get', 'test-results', 'summary', '--path', bundle],
      { encoding: 'utf8', timeout: 60_000, maxBuffer: 32 * 1024 * 1024 },
    )
  } catch {
    return ''
  }
}

/**
 * Copies the result bundles under `output`, a summary of each, and the crash
 * reports written since `since` (epoch milliseconds) into `destination`.
 * Returns what it kept. Anything that cannot be copied is skipped: this runs
 * after a failure and must never replace that failure with its own.
 */
export const collectDiagnostics = ({
  output,
  destination,
  since,
  reportsDir = join(homedir(), 'Library', 'Logs', 'DiagnosticReports'),
  summarize = summarizeBundle,
}) => {
  const kept = []
  mkdirSync(destination, { recursive: true })
  for (const name of ['results.xcresult', 'activation.xcresult']) {
    const from = join(output, name)
    if (!existsSync(from)) continue
    try {
      cpSync(from, join(destination, name), { recursive: true })
      kept.push(name)
    } catch {
      continue
    }
    const summary = summarize(from)
    if (summary) {
      writeFileSync(join(destination, `${name}.summary.json`), summary)
      kept.push(`${name}.summary.json`)
    }
  }
  let reports = []
  try {
    reports = readdirSync(reportsDir).filter(
      (file) =>
        /\.(ips|crash)$/.test(file) &&
        statSync(join(reportsDir, file)).mtimeMs >= since,
    )
  } catch {
    // No reports folder: nothing crashed on this machine.
  }
  if (reports.length > 0) mkdirSync(join(destination, 'crash-reports'))
  for (const file of reports) {
    try {
      cpSync(join(reportsDir, file), join(destination, 'crash-reports', file))
      kept.push(`crash-reports/${file}`)
    } catch {
      continue
    }
  }
  return kept
}
