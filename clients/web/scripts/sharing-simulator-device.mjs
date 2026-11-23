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
