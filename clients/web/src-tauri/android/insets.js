// Runs at the start of every page load in the Android shell (lib.rs).
//
// `MainActivity.kt` (android/) exposes `AxonInsets.get()` -> "top,right,bottom,left"
// in CSS pixels, and calls `__axonApplyInsets` when they change. Set as inline
// properties on :root, which is what ADR 0105's `--safe-*` tokens are for; the
// e2e spec does exactly the same thing.
;(function () {
  function apply() {
    var root = document.documentElement
    if (!root || !window.AxonInsets) return
    var v = String(window.AxonInsets.get()).split(',')
    var names = ['top', 'right', 'bottom', 'left']
    for (var i = 0; i < 4; i++) {
      var px = parseFloat(v[i])
      if (isFinite(px)) root.style.setProperty('--safe-' + names[i], px + 'px')
    }
  }
  window.__axonApplyInsets = apply
  apply()
  // The script can run before <html> exists; try again once it does.
  document.addEventListener('DOMContentLoaded', apply)
})()
