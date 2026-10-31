import { execFileSync } from 'node:child_process'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

// Run the real shared-module XCTest suite under iOS, without installing or
// changing Axon itself. Signing and physical Keychain acceptance stay separate.
const source = fileURLToPath(
  new URL('../src-tauri/incoming-share/', import.meta.url),
)
const output = mkdtempSync(join(tmpdir(), 'axon-sharing-simulator-'))
const run = (command, args, timeout = 300_000) =>
  execFileSync(command, args, { stdio: 'inherit', timeout })
const sdk = execFileSync(
  'xcrun',
  ['--sdk', 'iphonesimulator', '--show-sdk-version'],
  {
    encoding: 'utf8',
    timeout: 15_000,
  },
).trim()
const devices = Object.entries(
  JSON.parse(
    execFileSync('xcrun', ['simctl', 'list', 'devices', 'available', '-j'], {
      encoding: 'utf8',
      timeout: 15_000,
    }),
  ).devices,
).flatMap(([runtime, devices]) =>
  devices.map((device) => ({ ...device, runtime })),
)
const requested = process.env.AXON_SHARING_SIMULATOR
const phones = devices.filter((device) => device.name.startsWith('iPhone'))
const matching = phones.filter(
  (device) =>
    device.runtime.split('.iOS-')[1]?.split('-').slice(0, 2).join('.') ===
    sdk.split('.').slice(0, 2).join('.'),
)
const device = requested
  ? devices.find((device) => device.udid === requested)
  : (matching.find((device) => device.state === 'Booted') ??
    matching[0] ??
    phones.find((device) => device.state === 'Booted') ??
    phones[0])
if (device === undefined)
  throw new Error(
    'No available iPhone simulator. Install an iOS runtime in Xcode; optionally set AXON_SHARING_SIMULATOR to its UDID.',
  )
console.log(
  `Simulator SDK ${sdk}; selected ${device.name} (${device.runtime}).`,
)
if (device.state !== 'Booted')
  run('xcrun', ['simctl', 'boot', device.udid], 30_000)
// A fresh CI simulator migrates OS data on first boot; allow the same bounded
// five-minute budget as the test build instead of timing out mid-migration.
run('xcrun', ['simctl', 'bootstatus', device.udid, '-b'])
const project = {
  name: 'SharingFoundationTests',
  options: { deploymentTarget: { iOS: '15.0' } },
  packages: { AxonIncomingShare: { path: source } },
  targets: {
    FoundationTests: {
      type: 'bundle.unit-test',
      platform: 'iOS',
      sources: [join(source, 'Tests/AxonIncomingShareTests')],
      dependencies: [{ package: 'AxonIncomingShare' }],
      settings: {
        base: {
          PRODUCT_BUNDLE_IDENTIFIER: 'org.matrixaxon.foundation.tests',
          GENERATE_INFOPLIST_FILE: 'YES',
          CODE_SIGNING_ALLOWED: 'NO',
          SWIFT_VERSION: '5.0',
        },
      },
    },
  },
  schemes: {
    FoundationTests: {
      build: { targets: { FoundationTests: 'test' } },
      test: { targets: ['FoundationTests'] },
    },
  },
}
writeFileSync(join(output, 'project.yml'), JSON.stringify(project))
run('xcodegen', ['generate', '--spec', join(output, 'project.yml')], 30_000)
console.log(
  `Testing on ${device.name}. Results: ${join(output, 'results.xcresult')}`,
)
run('xcodebuild', [
  'test',
  '-project',
  join(output, 'SharingFoundationTests.xcodeproj'),
  '-scheme',
  'FoundationTests',
  '-destination',
  `platform=iOS Simulator,id=${device.udid}`,
  '-derivedDataPath',
  join(output, 'DerivedData'),
  '-resultBundlePath',
  join(output, 'results.xcresult'),
  'CODE_SIGNING_ALLOWED=NO',
])
