import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

import { isVersion, minimumIosVersion } from './ios-minimum-version.mjs'

const tauriDir = (files) => {
  const dir = mkdtempSync(join(tmpdir(), 'ios-minimum-version-test-'))
  for (const [name, body] of Object.entries(files))
    writeFileSync(join(dir, name), JSON.stringify(body))
  return dir
}
const config = (version) => ({
  bundle: { iOS: { minimumSystemVersion: version } },
})

test('reads the version from tauri.conf.json', () => {
  const dir = tauriDir({ 'tauri.conf.json': config('26.0') })
  assert.equal(minimumIosVersion(dir), '26.0')
  rmSync(dir, { recursive: true })
})

test('tauri.ios.conf.json overrides it, as Tauri layers them', () => {
  const dir = tauriDir({
    'tauri.conf.json': config('26.0'),
    'tauri.ios.conf.json': config('18.4'),
  })
  assert.equal(minimumIosVersion(dir), '18.4')
  rmSync(dir, { recursive: true })
})

test('an ios file that does not set it leaves the base value', () => {
  const dir = tauriDir({
    'tauri.conf.json': config('26.0'),
    'tauri.ios.conf.json': { bundle: { iOS: { developmentTeam: 'X' } } },
  })
  assert.equal(minimumIosVersion(dir), '26.0')
  rmSync(dir, { recursive: true })
})

test('a missing or malformed version is an error, not a silent default', () => {
  for (const files of [
    {},
    { 'tauri.conf.json': {} },
    { 'tauri.conf.json': config('26') },
    { 'tauri.conf.json': config('26.0.1') },
    { 'tauri.conf.json': config(26) },
    { 'tauri.conf.json': config('latest') },
  ]) {
    const dir = tauriDir(files)
    assert.throws(() => minimumIosVersion(dir), /major.minor/)
    rmSync(dir, { recursive: true })
  }
  assert.ok(isVersion('26.0') && isVersion('15.0') && !isVersion('26'))
})

test("the repository's own configuration is valid, and the CLI prints it", () => {
  const script = fileURLToPath(
    new URL('./ios-minimum-version.mjs', import.meta.url),
  )
  const printed = execFileSync('node', [script], { encoding: 'utf8' }).trim()
  assert.ok(isVersion(printed), printed)
})
