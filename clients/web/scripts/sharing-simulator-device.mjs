// Which simulator the share-sheet UI tests run on.
//
// The tests were written against a phone-sized screen, and scrolling, sheet
// layout and where the composer's Cancel and Send buttons sit all depend on it.
// Taking the first iPhone on the runtime picked "iPhone 17 Pro" under Xcode 26
// and "iPhone Duo" under Xcode 27: a 466 x 678 point screen where the composer
// is an inset sheet with no navigation bar in view, so four photo-composer tests
// failed to find Cancel or Send while the app itself was working. So a standard
// model is preferred, and anything unfamiliar is a last resort.

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
