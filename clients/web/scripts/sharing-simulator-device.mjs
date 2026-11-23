// Which simulator the share-sheet UI tests run on.
//
// The tests were written against a phone-sized screen, and scrolling, sheet
// layout and where the composer's Cancel and Send buttons sit all depend on it.
// Taking the first iPhone on the runtime picked "iPhone 17 Pro" under Xcode 26
// and "iPhone Duo" under Xcode 27: a 466 x 678 point screen where the composer
// is an inset sheet with no navigation bar in view, so four photo-composer tests
// failed to find Cancel or Send while the app itself was working. So a standard
// model is preferred, and anything unfamiliar is a last resort.
//
// scripts/ci/ios-launch-check.sh chooses its simulator the same way, through the
// `pick` command at the bottom of this file, so there is one ranking and it is tested.

import { execFileSync } from 'node:child_process'

import { isEntryPoint } from './is-entry-point.mjs'

/** 0 for "iPhone 17 Pro" style names, up to 3 for a model these tests were not made for. */
export const rankDevice = (name) => {
  if (/^iPhone \d+ Pro$/.test(name)) return 0
  if (/^iPhone \d+$/.test(name)) return 1
  if (/^iPhone \d+/.test(name)) return 2 // Pro Max, Plus, e
  return 3 // Air, SE, Duo, and whatever Apple adds next
}

const modelNumber = (name) => Number(name.match(/^iPhone (\d+)/)?.[1] ?? 0)

/**
 * The best of `candidates` ({ name, state, udid }): the most standard model, then a
 * booted one (it costs no boot), then the newest model, then the name for a stable
 * answer. Undefined for none.
 */
export const pickPhone = (candidates) =>
  [...candidates].sort(
    (a, b) =>
      rankDevice(a.name) - rankDevice(b.name) ||
      (b.state === 'Booted') - (a.state === 'Booted') ||
      modelNumber(b.name) - modelNumber(a.name) ||
      a.name.localeCompare(b.name),
  )[0]

/**
 * A new simulator of the most standard iPhone model the runtime supports, for when
 * the runtime ships only models the tests were not made for (the Xcode 27 runtime
 * ships "iPhone Duo" and nothing else). `exec(args)` runs `xcrun simctl <args>` and
 * returns its output. Returns { name, udid, state, runtime } or undefined: no
 * standard model is supported, or simctl would not create one, and the caller
 * keeps what it had.
 */
export const createStandardPhone = ({ runtime, exec, log = console }) => {
  try {
    const listed = JSON.parse(exec(['list', 'runtimes', '-j'])).runtimes
    const types = (
      listed.find((entry) => entry.identifier === runtime)
        ?.supportedDeviceTypes ?? []
    ).filter((type) => type.name.startsWith('iPhone'))
    log.log(
      `Device types on ${runtime}: ${types.map((type) => type.name).join(', ') || '(none)'}`,
    )
    const type = pickPhone(types)
    if (type === undefined || rankDevice(type.name) >= 3) return undefined
    const udid = exec([
      'create',
      `Axon share tests (${type.name})`,
      type.identifier,
      runtime,
    ]).trim()
    if (!udid) return undefined
    return { name: type.name, udid, state: 'Shutdown', runtime }
  } catch (error) {
    log.warn(`Could not create a standard iPhone simulator: ${error.message}`)
    return undefined
  }
}

const majorMinor = (version) => version.split('.').slice(0, 2).join('.')

/**
 * Where to launch on iOS `want`: the most standard iPhone already installed on any
 * runtime of that version, or else a device type to make one from. More than one
 * runtime can share a version: the Xcode 27.2 image has iOS-27-2 and, listed first,
 * iOS-27-2-iPhone19-4, which is for the iPhone Duo alone, so choosing across all of
 * them is what keeps the Duo from being taken because it came first.
 *
 * `runtimes` and `devices` are the objects `simctl list runtimes -j` and
 * `simctl list devices available -j` hold. Returns one of
 * { kind: 'device', udid, name }, { kind: 'create', type, runtime, name }, { kind: 'none' }.
 *
 * With `fresh`, an installed device is never returned: the answer is always a device
 * type to make a new simulator from. Whoever installs an app on the result and removes
 * it again must not be handed a simulator someone else already uses.
 */
export const pickSimulator = ({ runtimes, devices, want, fresh = false }) => {
  const matching = runtimes.filter(
    (runtime) =>
      runtime.isAvailable !== false &&
      runtime.identifier?.startsWith(
        'com.apple.CoreSimulator.SimRuntime.iOS',
      ) &&
      majorMinor(runtime.version) === majorMinor(want),
  )
  const installed = fresh
    ? undefined
    : pickPhone(
        matching.flatMap((runtime) =>
          (devices[runtime.identifier] ?? []).filter((device) =>
            device.name.startsWith('iPhone'),
          ),
        ),
      )
  if (installed)
    return { kind: 'device', udid: installed.udid, name: installed.name }
  const type = pickPhone(
    matching.flatMap((runtime) =>
      (runtime.supportedDeviceTypes ?? [])
        .filter((candidate) => candidate.name.startsWith('iPhone'))
        .map((candidate) => ({ ...candidate, runtime: runtime.identifier })),
    ),
  )
  if (type)
    return {
      kind: 'create',
      type: type.identifier,
      runtime: type.runtime,
      name: type.name,
    }
  return { kind: 'none' }
}

// `node sharing-simulator-device.mjs pick 26.0 [--new]` asks the simulators installed here
// and prints "device <udid> <name>", "create <type> <runtime> <name>" or "none"; with
// `--new` it only ever answers "create" or "none".
if (isEntryPoint(import.meta.url) && process.argv[2] === 'pick') {
  const simctl = (...args) =>
    JSON.parse(
      execFileSync('xcrun', ['simctl', ...args, '-j'], {
        encoding: 'utf8',
        timeout: 60_000,
        maxBuffer: 32 * 1024 * 1024,
      }),
    )
  const pick = pickSimulator({
    runtimes: simctl('list', 'runtimes').runtimes,
    devices: simctl('list', 'devices', 'available').devices,
    want: process.argv[3] ?? '',
    fresh: process.argv.includes('--new'),
  })
  console.log(
    pick.kind === 'device'
      ? `device ${pick.udid} ${pick.name}`
      : pick.kind === 'create'
        ? `create ${pick.type} ${pick.runtime} ${pick.name}`
        : 'none',
  )
}
