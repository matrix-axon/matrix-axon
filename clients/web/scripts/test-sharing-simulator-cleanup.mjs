import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { fileURLToPath } from 'node:url'

// test-sharing-simulator.mjs may make a simulator of its own, and it has to be removed
// however the script ends: it was removed only when the test run itself finished, so a
// failure while booting it or generating the project, before that, left one behind each
// time. These run the real script with a stub `xcrun` that answers from canned JSON,
// makes `simctl boot` fail, and records every call.

const script = fileURLToPath(
  new URL('./test-sharing-simulator.mjs', import.meta.url),
)

const runtime = 'com.apple.CoreSimulator.SimRuntime.iOS-27-1'
const types = [
  { name: 'iPhone 18 Pro', identifier: 'type.iPhone-18-Pro' },
  { name: 'iPhone Duo', identifier: 'type.iPhone-Duo' },
]

// The stub's answers, as the shell script that stands in for xcrun reads them.
const stub = ({ installedPhone }) => `#!/bin/sh
echo "$*" >> "$STUB_CALLS"
case "$*" in
  "--sdk iphonesimulator --show-sdk-version") echo 27.1 ;;
  "simctl list devices available -j") cat <<'JSON'
${JSON.stringify({ devices: { [runtime]: [installedPhone] } })}
JSON
    ;;
  "simctl list runtimes -j") cat <<'JSON'
${JSON.stringify({ runtimes: [{ identifier: runtime, version: '27.1', isAvailable: true, supportedDeviceTypes: types }] })}
JSON
    ;;
  "simctl create "*) echo MADE-UDID ;;
  "simctl boot "*) echo "Unable to boot" >&2; exit 1 ;;
esac
`

const run = (installedPhone) => {
  const dir = mkdtempSync(join(tmpdir(), 'sharing-simulator-cleanup-'))
  const calls = join(dir, 'calls.log')
  writeFileSync(calls, '')
  writeFileSync(join(dir, 'xcrun'), stub({ installedPhone }))
  chmodSync(join(dir, 'xcrun'), 0o755)
  const env = { ...process.env, PATH: `${dir}:${process.env.PATH}` }
  env.STUB_CALLS = calls
  delete env.AXON_SHARING_SIMULATOR
  delete env.AXON_SHARING_DIAGNOSTICS
  const result = spawnSync(process.execPath, [script], {
    env,
    encoding: 'utf8',
    timeout: 60_000,
  })
  const recorded = readFileSync(calls, 'utf8').trim().split('\n')
  rmSync(dir, { recursive: true })
  return { result, recorded }
}

test(
  'a simulator the script made is shut down and deleted when booting it fails',
  { skip: process.platform === 'win32' },
  () => {
    // The only installed iPhone is the Duo, so a standard one is made; booting it
    // fails, which is before the try that used to be the only place it was removed.
    const { result, recorded } = run({
      name: 'iPhone Duo',
      udid: 'DUO-UDID',
      state: 'Shutdown',
    })
    assert.notEqual(result.status, 0, 'the script should fail when boot fails')
    assert.ok(
      recorded.includes(
        'simctl create Axon share tests (iPhone 18 Pro) type.iPhone-18-Pro ' +
          runtime,
      ),
      recorded.join('\n'),
    )
    assert.ok(recorded.includes('simctl boot MADE-UDID'), recorded.join('\n'))
    assert.ok(
      recorded.includes('simctl shutdown MADE-UDID'),
      recorded.join('\n'),
    )
    assert.ok(recorded.includes('simctl delete MADE-UDID'), recorded.join('\n'))
    // The Duo was somebody's: it is never shut down or deleted.
    assert.ok(
      !recorded.some((call) => /(shutdown|delete) DUO-UDID/.test(call)),
      recorded.join('\n'),
    )
  },
)

test(
  'an installed simulator is never shut down or deleted when nothing was made',
  { skip: process.platform === 'win32' },
  () => {
    const { result, recorded } = run({
      name: 'iPhone 17 Pro',
      udid: 'OWN-UDID',
      state: 'Shutdown',
    })
    assert.notEqual(result.status, 0)
    assert.ok(recorded.includes('simctl boot OWN-UDID'), recorded.join('\n'))
    assert.ok(
      !recorded.some((call) => /^simctl (create|shutdown|delete)/.test(call)),
      recorded.join('\n'),
    )
  },
)
