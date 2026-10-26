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

/** The page's user-activation state, as `copyText` found it on entry. */
interface Activation {
  isActive: boolean | null
  hasBeenActive: boolean | null
}

/**
 * Snapshot before anything is awaited. After the shell's dynamic import and
 * IPC round trip the activation has usually lapsed, so reading it later would
 * report the wrong state for the gesture that asked for the copy.
 */
function readActivation(): Activation {
  const state = (
    navigator as {
      userActivation?: { isActive: boolean; hasBeenActive: boolean }
    }
  ).userActivation
  return {
    isActive: state?.isActive ?? null,
    hasBeenActive: state?.hasBeenActive ?? null,
  }
}

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
  activation: Activation,
  extra?: CopyDetail,
): void {
  perfMark('copy:failed', {
    ...extra,
    source,
    reason,
    error: err instanceof Error ? err.name : typeof err,
    activation: activation.isActive,
    everActive: activation.hasBeenActive,
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
 * demand so the browser build never loads it. Resolves false, never throws,
 * when the plugin cannot be loaded or refuses the write; `copyText` then tries
 * the web path.
 *
 * `stage` in the failure mark separates a chunk that did not load from a write
 * the plugin refused (a missing capability, say), which want different fixes.
 */
async function nativeCopy(
  text: string,
  source: CopySource,
  activation: Activation,
  extra?: CopyDetail,
): Promise<boolean> {
  let stage: 'load' | 'write' = 'load'
  try {
    const { writeText } = await import('@tauri-apps/plugin-clipboard-manager')
    stage = 'write'
    await writeText(text)
    perfMark('copy:ok', {
      ...extra,
      source,
      native: true,
      activation: activation.isActive,
    })
    return true
  } catch (err) {
    perfMark('copy:native-failed', {
      ...extra,
      source,
      stage,
      error: err instanceof Error ? err.name : typeof err,
      activation: activation.isActive,
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
  const activation = readActivation()
  // Only the shell awaits before the web write. A browser must reach
  // `writeText` synchronously, inside the gesture that called us. After a
  // failed native attempt the web write runs later than that and WKWebView
  // will often refuse it, so the fallback is a last chance, not a guarantee.
  if (isTauriRuntime() && (await nativeCopy(text, source, activation, extra))) {
    return true
  }
  if (navigator.clipboard?.writeText === undefined) {
    markCopyFailure(source, 'no-api', null, activation, extra)
    const recovered = execCommandCopy(text)
    perfMark('copy:fallback', { source, recovered })
    return recovered
  }
  try {
    await navigator.clipboard.writeText(text)
    // Successes are marked too, or a log of failures cannot say which source
    // wins and how often.
    perfMark('copy:ok', {
      ...extra,
      source,
      activation: activation.isActive,
    })
    return true
  } catch (err) {
    markCopyFailure(source, 'rejected', err, activation, extra)
    // The failure is marked either way; `fallback` says whether it mattered.
    const recovered = execCommandCopy(text)
    perfMark('copy:fallback', { source, recovered })
    return recovered
  }
}
