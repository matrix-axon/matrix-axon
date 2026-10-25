import { perfMark } from './perf'
import { isTauriRuntime } from './platform'

/** Where a copy was triggered from; a closed set, since it is persisted. */
export type CopySource =
  | 'link-click'
  | 'body-dblclick'
  | 'body-retry-click'
  | 'body-hold-touchend'
  | 'body-hold-click'
  | 'body-hold-release'
  | 'body-hold-cancel'
  | 'other'

/** Caller-supplied context for the marks; numbers and flags only. */
export type CopyDetail = Record<string, string | number | boolean | null>

/**
 * Why a copy failed, as a `copy:failed` mark. The mark is the only trace of a
 * rejection — the UI says "Copy failed" and nothing else — and the reason
 * matters: `NotAllowedError` with `activation: false` is a gesture problem, the
 * same error with `activation: true` is a permission one, and a missing API or
 * insecure context never reaches the write at all. Carries no clipboard text.
 */
function markCopyFailure(
  source: CopySource,
  reason: string,
  err: unknown,
  extra?: CopyDetail,
): void {
  const activation = (
    navigator as {
      userActivation?: { isActive: boolean; hasBeenActive: boolean }
    }
  ).userActivation
  perfMark('copy:failed', {
    ...extra,
    source,
    reason,
    error: err instanceof Error ? err.name : typeof err,
    activation: activation?.isActive ?? null,
    everActive: activation?.hasBeenActive ?? null,
    focused: document.hasFocus(),
    visible: document.visibilityState,
    secure: window.isSecureContext,
  })
}

/**
 * The pre-Clipboard-API route: select a throwaway textarea and `execCommand`.
 * It is synchronous and tolerant of the focus/activation state that makes
 * `writeText` reject, which is why it is the second attempt rather than the
 * first. The textarea is read-only (no iOS keyboard) and off-screen.
 */
function execCommandCopy(text: string): boolean {
  if (typeof document.execCommand !== 'function') {
    return false
  }
  const area = document.createElement('textarea')
  area.value = text
  area.readOnly = true
  area.setAttribute('aria-hidden', 'true')
  area.style.cssText =
    'position:fixed;top:0;left:-9999px;opacity:0;pointer-events:none'
  document.body.append(area)
  try {
    area.select()
    area.setSelectionRange(0, text.length)
    return document.execCommand('copy')
  } catch {
    return false
  } finally {
    area.remove()
  }
}

/**
 * The shell's clipboard plugin, which has no user-activation rule. Imported on
 * demand so the browser build never loads it, and `null` outside the shell or
 * when the write fails — the caller then falls back to the web path, so a
 * missing capability degrades to the old behaviour instead of losing the copy.
 */
async function nativeCopy(text: string, source: CopySource): Promise<boolean> {
  try {
    const { writeText } = await import('@tauri-apps/plugin-clipboard-manager')
    await writeText(text)
    perfMark('copy:ok', { source, native: true })
    return true
  } catch (err) {
    perfMark('copy:native-failed', {
      source,
      error: err instanceof Error ? err.name : typeof err,
    })
    return false
  }
}

/** Write `text` to the system clipboard. False when the write is rejected and
 *  the `execCommand` fallback fails too (permissions, insecure context). */
export async function copyText(
  text: string,
  source: CopySource = 'other',
  extra?: CopyDetail,
): Promise<boolean> {
  // Only the shell awaits before the web write. A browser must reach
  // `writeText` synchronously, inside the gesture that called us.
  if (isTauriRuntime() && (await nativeCopy(text, source))) {
    return true
  }
  if (navigator.clipboard?.writeText === undefined) {
    markCopyFailure(source, 'no-api', null, extra)
    const recovered = execCommandCopy(text)
    perfMark('copy:fallback', { source, recovered })
    return recovered
  }
  // Read before the await: this is the state the browser judged the write by.
  const activation = navigator.userActivation?.isActive ?? null
  try {
    await navigator.clipboard.writeText(text)
    // Successes are marked too, or a log of failures cannot say which source
    // wins and how often.
    perfMark('copy:ok', { ...extra, source, activation })
    return true
  } catch (err) {
    markCopyFailure(source, 'rejected', err, extra)
    // The failure is marked either way; `fallback` says whether it mattered.
    const recovered = execCommandCopy(text)
    perfMark('copy:fallback', { source, recovered })
    return recovered
  }
}
