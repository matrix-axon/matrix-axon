package org.matrixaxon.axon

import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

/**
 * Replaces the MainActivity `tauri android init` generates; see
 * `scripts/package-android.sh`, which copies it over `gen/android` on every run
 * (the generated project is gitignored and regenerated, so an edit made there
 * does not survive).
 *
 * The generated one calls `enableEdgeToEdge()` and does nothing about the
 * insets it then owes: the page is drawn under the status and navigation bars
 * and is left to keep clear of them with `env(safe-area-inset-*)`. Android
 * System WebView only resolves that to a real value from a recent Chrome, so
 * on an older one — an API 33 emulator image carries 109 — it is 0px and the
 * top bar sits under the clock. Targeting API 35 makes edge-to-edge mandatory
 * on Android 15, so turning it off is not an option there either.
 *
 * So the insets come from here instead. ADR 0105 made `--safe-top` and its
 * three siblings the only readers of `env()` for this reason: setting them on
 * `:root` is enough, with no change to any rule that uses them. `insets.js`,
 * injected into every page load by `lib.rs`, reads `AxonInsets.get()` and does
 * that; this file pushes each change through `__axonApplyInsets`.
 *
 * Values are CSS pixels: the WebView lays out in density-independent units, so
 * a raw pixel inset would be `density` times too large.
 */
class MainActivity : TauriActivity() {
  @Volatile
  private var insets = Insets(0f, 0f, 0f, 0f)

  private data class Insets(val top: Float, val right: Float, val bottom: Float, val left: Float)

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)

    webView.addJavascriptInterface(
      object {
        @JavascriptInterface
        fun get(): String = "${insets.top},${insets.right},${insets.bottom},${insets.left}"
      },
      "AxonInsets"
    )

    ViewCompat.setOnApplyWindowInsetsListener(webView) { view, windowInsets ->
      // System bars and the display cutout are the fixed strips. The keyboard
      // is not a strip the page can ignore, and counts as bottom inset while it
      // is up so the composer rises above it rather than under it.
      val fixed = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      val ime = windowInsets.getInsets(WindowInsetsCompat.Type.ime())
      val density = view.resources.displayMetrics.density
      insets = Insets(
        fixed.top / density,
        fixed.right / density,
        maxOf(fixed.bottom, ime.bottom) / density,
        fixed.left / density
      )
      webView.evaluateJavascript("window.__axonApplyInsets && window.__axonApplyInsets()", null)
      // Not consumed: the WebView's own handling still runs.
      windowInsets
    }
    ViewCompat.requestApplyInsets(webView)
  }
}
