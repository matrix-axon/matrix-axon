import assert from 'node:assert/strict'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { collectDiagnostics } from './sharing-diagnostics.mjs'

const fixture = () => {
  const root = mkdtempSync(join(tmpdir(), 'sharing-diagnostics-test-'))
  const output = join(root, 'output')
  const reportsDir = join(root, 'reports')
  mkdirSync(join(output, 'activation.xcresult'), { recursive: true })
  writeFileSync(join(output, 'activation.xcresult', 'Info.plist'), 'bundle')
  mkdirSync(reportsDir)
  return { root, output, reportsDir, destination: join(root, 'kept') }
}
const report = (reportsDir, name, ageSeconds) => {
  const path = join(reportsDir, name)
  writeFileSync(path, 'crash')
  const when = new Date(Date.now() - ageSeconds * 1000)
  utimesSync(path, when, when)
}

test('keeps the bundles that exist, with a summary of each', () => {
  const { root, output, reportsDir, destination } = fixture()
  const summarized = []
  const kept = collectDiagnostics({
    output,
    destination,
    since: 0,
    reportsDir,
    summarize: (bundle) => {
      summarized.push(bundle)
      return '{"result":"Failed"}'
    },
  })
  assert.deepEqual(kept, [
    'activation.xcresult',
    'activation.xcresult.summary.json',
  ])
  assert.equal(
    readFileSync(
      join(destination, 'activation.xcresult', 'Info.plist'),
      'utf8',
    ),
    'bundle',
  )
  assert.equal(
    readFileSync(join(destination, 'activation.xcresult.summary.json'), 'utf8'),
    '{"result":"Failed"}',
  )
  assert.deepEqual(summarized, [join(output, 'activation.xcresult')])
  rmSync(root, { recursive: true })
})

test('a bundle Xcode cannot summarize is still kept', () => {
  const { root, output, reportsDir, destination } = fixture()
  const kept = collectDiagnostics({
    output,
    destination,
    since: 0,
    reportsDir,
    summarize: () => '',
  })
  assert.deepEqual(kept, ['activation.xcresult'])
  rmSync(root, { recursive: true })
})

test('keeps only crash reports written since the run began', () => {
  const { root, output, reportsDir, destination } = fixture()
  report(reportsDir, 'ShareSource-new.ips', 5)
  report(reportsDir, 'Other-new.crash', 5)
  report(reportsDir, 'ShareSource-old.ips', 3600)
  report(reportsDir, 'notes.txt', 5)
  const kept = collectDiagnostics({
    output,
    destination,
    since: Date.now() - 60_000,
    reportsDir,
    summarize: () => '',
  })
  assert.deepEqual(readdirSync(join(destination, 'crash-reports')).sort(), [
    'Other-new.crash',
    'ShareSource-new.ips',
  ])
  assert.equal(
    kept.filter((name) => name.startsWith('crash-reports/')).length,
    2,
  )
  rmSync(root, { recursive: true })
})

test('a run that left nothing yields an empty directory, not an error', () => {
  const root = mkdtempSync(join(tmpdir(), 'sharing-diagnostics-test-'))
  const destination = join(root, 'kept')
  const kept = collectDiagnostics({
    output: join(root, 'missing'),
    destination,
    since: 0,
    reportsDir: join(root, 'no-reports-folder'),
    summarize: () => '',
  })
  assert.deepEqual(kept, [])
  assert.ok(existsSync(destination))
  assert.equal(existsSync(join(destination, 'crash-reports')), false)
  rmSync(root, { recursive: true })
})
