import { signal, type ReadonlySignal } from '@preact/signals'
import type { ApiClient } from './api/client'
import type { EditPreviewDraft, EditPreviewSeed } from './edit-link-previews'
import { matchUrls } from './html/linkify'
import type { LinkPreview } from './link-previews'
import type {
  OutgoingLinkPreview,
  PreviewContent,
  PreviewThumbnail,
} from './link-preview-send'
import {
  LinkPreviewError,
  type DeviceLinkPreview,
  type LinkPreviewFailure,
} from './platform'
import { isMatrixLink } from './matrix-to'
import {
  isPreviewLink,
  linkIdentity,
  PREVIEW_FIELD_LIMITS,
  previewField,
} from './preview-links'
import type { LinkPreviewSetting } from './stores/settings'

/**
 * The composer's link-preview cards for one draft (ADR 0132 § 1, 2 and 5).
 *
 * Plain state over signals, so it is testable without rendering. The
 * composer reports every edit; this decides which links have cards, starts
 * and cancels lookups, and hands a send the cards the user kept. One
 * instance belongs to one draft, and `take` starts the next one.
 *
 * An edit's draft starts from the message's own cards (§ 8): they show under
 * any setting, because removing one fetches nothing, and the message's links
 * never get new cards. Links the edit adds follow the setting, while the edit
 * has fewer than three cards in all.
 */

/** Cards per message (§ 1); a fourth link gets none. */
export const MAX_COMPOSER_PREVIEWS = 3
/** How long a typed link waits before its lookup starts (§ 5). */
export const TYPED_LINK_DELAY_MS = 400
/** Results kept in memory, across drafts and rooms (§ 5). */
export const PREVIEW_CACHE_LIMIT = 20
/** The upload contract's ceiling on a thumbnail (`bundled-link-previews.md`). */
const MAX_THUMBNAIL_BYTES = 2 * 1024 * 1024

/** A card's content as either source returned it, normalized. */
export interface PreviewResult extends PreviewContent {
  /** The page named an image that could not be fetched or kept. */
  imageLost: boolean
}

/** `PreviewGenerationStatus` other than `ready`, plus a ready answer with nothing usable. */
export type HomeserverFailure =
  | 'unsupported'
  | 'disabled'
  | 'timeout'
  | 'unavailable'
  | 'busy'
  | 'no_metadata'

export type PreviewFailure =
  | { source: 'device'; kind: Exclude<LinkPreviewFailure, 'cancelled'> }
  | { source: 'homeserver'; kind: HomeserverFailure }

/** Why a source built no card. Never carries the link. */
export class PreviewLookupError extends Error {
  readonly name = 'PreviewLookupError'
  readonly failure: PreviewFailure
  constructor(failure: PreviewFailure) {
    super(`Link preview: ${failure.source} ${failure.kind}`)
    this.failure = failure
  }
}

export type CardState =
  /** A typed link, inside its delay. */
  | { status: 'waiting' }
  | { status: 'loading'; source: 'device' | 'homeserver' }
  | { status: 'ready'; result: PreviewResult }
  | { status: 'failed'; failure: PreviewFailure }
  /** No device source: the homeserver may be asked, and has not been. */
  | { status: 'offer' }
  /** A card the message being edited already has (§ 8). */
  | { status: 'kept'; preview: LinkPreview }

export interface ComposerPreviewCard {
  /** The link's identity (`linkIdentity`), which keys everything here. */
  identity: string
  /** The exact text of the link's first occurrence. */
  matchedUrl: string
  hostname: string
  state: CardState
  /** Whether the card offers to ask the homeserver now. */
  canAskHomeserver: boolean
}

/** Where cards come from, per the setting and the platform. */
export interface PreviewSources {
  setting: LinkPreviewSetting
  /** This device's fetcher (`Platform.linkPreview`); absent in a browser. */
  device?: (url: string, signal: AbortSignal) => Promise<DeviceLinkPreview>
  /** The homeserver, through Axon; asked only from a card's button. */
  homeserver?: (url: string, signal: AbortSignal) => Promise<PreviewResult>
}

/** This device may build cards: the setting permits it and the platform can. */
function deviceSourceOn(sources: PreviewSources): boolean {
  return sources.setting !== 'off' && sources.device != null
}

/** The homeserver may be offered: the setting permits it and it is wired. */
function homeserverSourceOn(sources: PreviewSources): boolean {
  return sources.setting === 'device+homeserver' && sources.homeserver != null
}

/**
 * Whether the composer authors previews at all (ADR 0132 § 4). False is the
 * setting `off`, including a stored `device` where there is no device source:
 * then no cards are made and a send leaves `url_previews` out (§ 6 rule 1).
 */
export function linkPreviewsEnabled(sources: PreviewSources): boolean {
  return deviceSourceOn(sources) || homeserverSourceOn(sources)
}

/**
 * Results by link identity, least recently used first. Shared by every
 * composer, so retyping or moving a link, or switching rooms, does not fetch
 * it again. Holds normalized metadata and thumbnail bytes, never an upload
 * id (§ 7). Wiped on sign-out.
 */
export interface LinkPreviewCache {
  get(identity: string): PreviewResult | undefined
  /**
   * How one lookup stores its result, taken when the lookup starts. A clear
   * since then makes it a no-op, so a result that was in flight across a
   * sign-out cannot land in the cache the next session reads. There is no
   * unfenced write.
   */
  writer(): (identity: string, result: PreviewResult) => void
  /** Forget every result, and every writer taken before now. */
  clear(): void
}

export function createLinkPreviewCache(
  limit = PREVIEW_CACHE_LIMIT,
): LinkPreviewCache {
  const entries = new Map<string, PreviewResult>()
  let generation = 0
  return {
    get(identity) {
      const result = entries.get(identity)
      if (result !== undefined) {
        entries.delete(identity)
        entries.set(identity, result)
      }
      return result
    },
    writer() {
      const taken = generation
      return (identity, result) => {
        if (taken !== generation) return
        entries.delete(identity)
        entries.set(identity, result)
        while (entries.size > limit)
          entries.delete(entries.keys().next().value!)
      }
    },
    clear() {
      // Synchronous, and writes no signal: it runs inside the sign-out
      // effect, and must be a barrier for writers already in flight.
      generation += 1
      entries.clear()
    },
  }
}

export interface ComposerLinkPreviews {
  cards: ReadonlySignal<readonly ComposerPreviewCard[]>
  /**
   * The draft has a plain `http://` link the user added, complete as a card's
   * would be, while previews are on. Only HTTPS links get a card (§ 1), so
   * the composer says why this one has none. Links of a restored draft, or of
   * the message being edited, never raise it.
   */
  plainHttp: ReadonlySignal<boolean>
  /**
   * Adopt `text` without adding any of its links: a restored draft, one
   * pushed from another device, or a new scope. Its links get no card and no
   * lookup until the user retypes or re-pastes them (§ 5). With `edit`, the
   * draft edits a message and starts from its cards (§ 8).
   */
  reset(text: string, edit?: EditPreviewSeed): void
  /**
   * Start again from `edit` (or leave edit mode, without one), keeping the
   * draft as it is: its text, and which of its links were pasted, so a
   * pasted link stays complete. A reopened or upgraded edit (§ 8).
   */
  reseed(edit?: EditPreviewSeed): void
  /**
   * An edit made the draft `text`. `inserted` is a paste or a drop, whose new
   * links are complete at once; a typed link is complete once whitespace
   * follows it.
   */
  edit(text: string, inserted?: boolean): void
  /**
   * The current setting and sources, and whether cards are suspended: a
   * staged attachment or a slash command (§ 5). Call it before `edit` when
   * an edit changes suspension, so the edit never starts a lookup the
   * suspension would forbid.
   */
  configure(sources: PreviewSources, suspended: boolean): void
  /** Remove a card, and remember its link for the rest of the draft. */
  discard(identity: string): void
  /** The user's explicit choice to disclose this link to the homeserver. */
  askHomeserver(identity: string): void
  /** Whether `text` holds a link this draft discarded. */
  hasDiscardedLink(text: string): boolean
  /** What a send needs from the draft, as it stands. `kept` and `opened`
   *  are empty unless the draft edits a message. */
  snapshot(): EditPreviewDraft
  /** `snapshot`, then start the next draft, empty. */
  take(): EditPreviewDraft
  dispose(): void
}

interface Link {
  identity: string
  matchedUrl: string
  complete: boolean
}

interface Entry {
  matchedUrl: string
  state: CardState
  /** A lookup in flight or scheduled; aborting it cancels either. */
  abort: AbortController | null
}

/**
 * Distinct eligible links in text order, with completeness. `kept` names
 * links that count whatever the fetch policy says: an edited message's own
 * cards, which the server keeps for any `http` or `https` link (§ 8).
 */
function scan(
  text: string,
  pasted: ReadonlySet<string>,
  kept: ReadonlySet<string> = new Set(),
): Link[] {
  const links = new Map<string, Link>()
  for (const match of matchUrls(text)) {
    // As the composer will send it (`lowercaseLinkSchemes`), so a card's
    // `matched_url` is the text the server finds in the body.
    const url = match.url.replace(/^https?:/i, (scheme) => scheme.toLowerCase())
    const { end } = match
    // A Matrix link gets no card and no lookup (ADR 0138 § 1), not even one
    // an edited message already carries: the server drops those from an edit.
    if (isMatrixLink(url)) continue
    const identity = linkIdentity(url)
    if (identity === null) continue
    if (!isPreviewLink(url) && !kept.has(identity)) continue
    // Whitespace after the link's own run of characters: typing can no
    // longer extend it, so the user has finished it.
    const complete = pasted.has(identity) || /^\S*\s/.test(text.slice(end))
    const known = links.get(identity)
    if (known === undefined)
      links.set(identity, { identity, matchedUrl: url, complete })
    else known.complete ||= complete
  }
  return [...links.values()]
}

/**
 * Every bare web link's identity, eligible or not, so that what was pasted or
 * restored is known for a plain `http://` link too. Cards still come only
 * from `scan`.
 */
function identities(text: string): Set<string> {
  return new Set(
    matchUrls(text).flatMap(({ url }) => {
      const identity = /^https?:/i.test(url) ? linkIdentity(url) : null
      return identity === null ? [] : [identity]
    }),
  )
}

/** The complete plain `http://` links in `text`, by identity. */
function plainHttpLinks(text: string, pasted: ReadonlySet<string>): string[] {
  return matchUrls(text).flatMap(({ url, end }) => {
    if (!/^http:\/\//i.test(url)) return []
    const identity = linkIdentity(url)
    return identity !== null &&
      (pasted.has(identity) || /^\S*\s/.test(text.slice(end)))
      ? [identity]
      : []
  })
}

function hostnameOf(identity: string): string {
  return new URL(identity).hostname
}

function sameCards(
  a: readonly ComposerPreviewCard[],
  b: readonly ComposerPreviewCard[],
): boolean {
  return (
    a.length === b.length &&
    a.every(
      (card, i) =>
        card.identity === b[i].identity &&
        card.matchedUrl === b[i].matchedUrl &&
        card.state === b[i].state &&
        card.canAskHomeserver === b[i].canAskHomeserver,
    )
  )
}

export function createComposerLinkPreviews(
  cache: LinkPreviewCache,
  {
    delayMs = TYPED_LINK_DELAY_MS,
  }: {
    delayMs?: number
  } = {},
): ComposerLinkPreviews {
  const cards = signal<readonly ComposerPreviewCard[]>([])
  const plainHttp = signal(false)
  let text = ''
  let restored = new Set<string>()
  let pasted = new Set<string>()
  // A ready preview keeps its link complete while the link stays in the draft.
  let previewed = new Set<string>()
  let discarded = new Set<string>()
  /** Editing a message: its cards by identity, and the links its text had,
   *  which never get new cards (§ 8). */
  let editing = false
  let addNew = true
  let existing = new Map<string, LinkPreview>()
  let original = new Set<string>()
  const entries = new Map<string, Entry>()
  let sources: PreviewSources = { setting: 'off' }
  let suspended = false
  let disposed = false

  const deviceOn = () => deviceSourceOn(sources)
  const homeserverOn = () => homeserverSourceOn(sources)
  const isComplete = (link: Link) =>
    link.complete || previewed.has(link.identity)

  function drop(identity: string) {
    entries.get(identity)?.abort?.abort()
    entries.delete(identity)
  }

  function dropAll() {
    for (const identity of [...entries.keys()]) drop(identity)
  }

  /** The links that hold a card slot: complete ones, in order, up to three.
   *  A restored or discarded link keeps its slot and shows no card, so a
   *  later link never moves up into it (§ 1). */
  function slotted(): Link[] {
    return scan(text, pasted).filter(isComplete).slice(0, MAX_COMPOSER_PREVIEWS)
  }

  function wanted(): Link[] {
    if (editing) return wantedInEdit()
    return slotted().filter(
      (link) => !restored.has(link.identity) && !discarded.has(link.identity),
    )
  }

  /** An edit's cards, in text order: the message's own, wherever their link
   *  still is, and new links' while the edit has fewer than three cards.
   *  None of the message's cards is ever dropped for the count (§ 8). */
  function wantedInEdit(): Link[] {
    const links = scan(text, pasted, new Set(existing.keys()))
    const shown = new Set(
      links
        .filter((l) => existing.has(l.identity) && !discarded.has(l.identity))
        .map((l) => l.identity),
    )
    const room = Math.max(0, MAX_COMPOSER_PREVIEWS - shown.size)
    // A discarded new link keeps its slot, as in a new message.
    const fresh = addNew
      ? links
          .filter(
            (l) =>
              isComplete(l) &&
              !original.has(l.identity) &&
              !existing.has(l.identity),
          )
          .slice(0, room)
          .filter((l) => !discarded.has(l.identity))
          .map((l) => l.identity)
      : []
    const keep = new Set([...shown, ...fresh])
    return links.filter((l) => keep.has(l.identity))
  }

  function settle(identity: string, entry: Entry, state: CardState) {
    if (disposed || entries.get(identity) !== entry) return
    entry.state = state
    entry.abort = null
    if (state.status === 'ready') previewed.add(identity)
    publish()
  }

  function lookUp(
    identity: string,
    entry: Entry,
    source: 'device' | 'homeserver',
  ) {
    const abort = new AbortController()
    entry.abort = abort
    entry.state = { status: 'loading', source }
    // Taken now, so a sign-out while this is in flight discards its result.
    const store = cache.writer()
    const run =
      source === 'device'
        ? sources.device!(entry.matchedUrl, abort.signal).then(fromDevice)
        : sources.homeserver!(entry.matchedUrl, abort.signal)
    run.then(
      (result) => {
        if (abort.signal.aborted) return
        store(identity, result)
        settle(identity, entry, { status: 'ready', result })
      },
      (error: unknown) => {
        if (abort.signal.aborted) return
        if (error instanceof LinkPreviewError && error.kind === 'cancelled') {
          // The plugin stopped it (a reload or a closed window): not a fact
          // about the link, so no error; the card waits to be asked again.
          drop(identity)
          publish()
          return
        }
        settle(identity, entry, {
          status: 'failed',
          failure: failureOf(error, source),
        })
      },
    )
  }

  function start(identity: string, link: Link, immediately: boolean) {
    const message = editing ? existing.get(identity) : undefined
    if (message !== undefined) {
      // Back in the text after a deletion: the message's card returns
      // without a lookup (§ 8).
      entries.set(identity, {
        matchedUrl: link.matchedUrl,
        state: { status: 'kept', preview: message },
        abort: null,
      })
      return
    }
    const cached = cache.get(identity)
    if (cached !== undefined) {
      previewed.add(identity)
      entries.set(identity, {
        matchedUrl: link.matchedUrl,
        state: { status: 'ready', result: cached },
        abort: null,
      })
      return
    }
    if (!deviceOn()) {
      entries.set(identity, {
        matchedUrl: link.matchedUrl,
        state: { status: 'offer' },
        abort: null,
      })
      return
    }
    const entry: Entry = {
      matchedUrl: link.matchedUrl,
      state: { status: 'waiting' },
      abort: null,
    }
    entries.set(identity, entry)
    if (immediately) {
      lookUp(identity, entry, 'device')
      return
    }
    const wait = new AbortController()
    entry.abort = wait
    const timer = setTimeout(() => {
      if (wait.signal.aborted || entries.get(identity) !== entry) return
      lookUp(identity, entry, 'device')
      publish()
    }, delayMs)
    wait.signal.addEventListener('abort', () => clearTimeout(timer), {
      once: true,
    })
  }

  function reconcile() {
    if (disposed) return
    const enabled = !suspended && linkPreviewsEnabled(sources)
    if (!enabled && !editing) {
      dropAll()
      publish()
      return
    }
    // Off, an edit keeps showing the message's cards and adds none.
    const links = wanted().filter(
      (link) => enabled || existing.has(link.identity),
    )
    const keep = new Set(links.map((link) => link.identity))
    for (const identity of [...entries.keys()])
      if (!keep.has(identity)) drop(identity)
    for (const link of links) {
      const entry = entries.get(link.identity)
      if (entry === undefined)
        start(link.identity, link, pasted.has(link.identity))
      else entry.matchedUrl = link.matchedUrl
    }
    publish()
  }

  function publish() {
    const next = wanted().flatMap((link): ComposerPreviewCard[] => {
      const entry = entries.get(link.identity)
      if (entry === undefined) return []
      const previous = cards.peek().find((c) => c.identity === link.identity)
      const card: ComposerPreviewCard = {
        identity: link.identity,
        matchedUrl: entry.matchedUrl,
        hostname: hostnameOf(link.identity),
        state: entry.state,
        canAskHomeserver:
          entry.state.status !== 'kept' &&
          homeserverOn() &&
          (entry.state.status === 'offer' || entry.state.status === 'failed'),
      }
      return [
        previous !== undefined && sameCards([previous], [card])
          ? previous
          : card,
      ]
    })
    if (!sameCards(cards.peek(), next)) cards.value = next
    // Only where an https:// link would get a card: not past the third, and
    // not in a media caption's edit (§ 8).
    const room = editing
      ? addNew && wantedInEdit().length < MAX_COMPOSER_PREVIEWS
      : slotted().length < MAX_COMPOSER_PREVIEWS
    const http =
      room &&
      !suspended &&
      linkPreviewsEnabled(sources) &&
      plainHttpLinks(text, pasted).some(
        (identity) =>
          !restored.has(identity) &&
          !original.has(identity) &&
          !existing.has(identity),
      )
    if (plainHttp.peek() !== http) plainHttp.value = http
  }

  function snapshot(): EditPreviewDraft {
    const shown = wanted()
    return {
      cards: shown.flatMap((link): OutgoingLinkPreview[] => {
        const state = entries.get(link.identity)?.state
        return state?.status === 'ready'
          ? [{ ...content(state.result), matchedUrl: link.matchedUrl }]
          : []
      }),
      kept: shown.flatMap((link) =>
        entries.get(link.identity)?.state.status === 'kept'
          ? [link.matchedUrl]
          : [],
      ),
      opened: new Set(existing.keys()),
      discarded: new Set(discarded),
    }
  }

  function reset(
    next: string,
    edit?: EditPreviewSeed,
    carriedPastes: ReadonlySet<string> = new Set(),
  ) {
    dropAll()
    previewed.clear()
    const present = identities(next)
    text = next
    pasted = new Set([...carriedPastes].filter((id) => present.has(id)))
    editing = edit !== undefined
    addNew = edit?.addNew ?? true
    existing = new Map(edit?.cards.map((card) => [card.url, card]) ?? [])
    original =
      edit === undefined
        ? new Set()
        : new Set([...identities(edit.originalText), ...(edit.refused ?? [])])
    restored = edit === undefined ? identities(next) : new Set()
    discarded = new Set(edit?.discarded ?? [])
    reconcile()
  }

  return {
    cards,
    plainHttp,
    reset,
    reseed(edit) {
      reset(text, edit, pasted)
    },
    edit(next, inserted = false) {
      if (next === text) return
      const before = identities(text)
      const after = identities(next)
      if (inserted)
        for (const identity of after)
          if (!before.has(identity)) pasted.add(identity)
      restored = new Set([...restored].filter((id) => after.has(id)))
      pasted = new Set([...pasted].filter((id) => after.has(id)))
      previewed = new Set([...previewed].filter((id) => after.has(id)))
      text = next
      reconcile()
    },
    configure(next, nextSuspended) {
      const changed =
        next.setting !== sources.setting ||
        (next.device == null) !== (sources.device == null) ||
        (next.homeserver == null) !== (sources.homeserver == null)
      sources = next
      if (changed) {
        // A card's state was decided under the old sources: keep only the
        // results and the message's own cards, and decide the rest again.
        for (const [identity, entry] of [...entries])
          if (entry.state.status !== 'ready' && entry.state.status !== 'kept')
            drop(identity)
      }
      if (changed || nextSuspended !== suspended) {
        suspended = nextSuspended
        reconcile()
      }
    },
    discard(identity) {
      drop(identity)
      discarded.add(identity)
      reconcile()
    },
    askHomeserver(identity) {
      const entry = entries.get(identity)
      if (
        entry === undefined ||
        !homeserverOn() ||
        (entry.state.status !== 'offer' && entry.state.status !== 'failed')
      )
        return
      lookUp(identity, entry, 'homeserver')
      publish()
    },
    hasDiscardedLink(candidate) {
      return [...identities(candidate)].some((id) => discarded.has(id))
    },
    snapshot,
    take() {
      const draft = snapshot()
      reset('')
      return draft
    },
    dispose() {
      dropAll()
      disposed = true
    },
  }
}

function content({
  title,
  description,
  siteName,
  image,
}: PreviewResult): PreviewContent {
  return { title, description, siteName, image }
}

function failureOf(
  error: unknown,
  source: 'device' | 'homeserver',
): PreviewFailure {
  if (error instanceof PreviewLookupError) return error.failure
  if (
    source === 'device' &&
    error instanceof LinkPreviewError &&
    error.kind !== 'cancelled'
  )
    return { source, kind: error.kind }
  return { source, kind: 'unavailable' } as PreviewFailure
}

/** Normalize text fields, or `null` when nothing is left to show. */
function normalized(
  fields: { title?: unknown; description?: unknown; siteName?: unknown },
  image: PreviewThumbnail | null,
  imageLost: boolean,
): PreviewResult | null {
  const result: PreviewResult = {
    title: previewField(fields.title, PREVIEW_FIELD_LIMITS.title),
    description: previewField(
      fields.description,
      PREVIEW_FIELD_LIMITS.description,
    ),
    siteName: previewField(fields.siteName, PREVIEW_FIELD_LIMITS.siteName),
    image,
    imageLost,
  }
  return result.title === null &&
    result.description === null &&
    result.siteName === null &&
    result.image === null
    ? null
    : result
}

/** A device result as a card's content, with the composer's own field rule. */
export function fromDevice(preview: DeviceLinkPreview): PreviewResult {
  const result = normalized(
    preview,
    preview.image === undefined
      ? null
      : { bytes: preview.image.bytes, mimeType: preview.image.mimeType },
    preview.imageUnavailable,
  )
  if (result === null)
    throw new PreviewLookupError({ source: 'device', kind: 'no_metadata' })
  return result
}

const THUMBNAIL_TYPES: ReadonlySet<string> = new Set([
  'image/png',
  'image/jpeg',
  'image/webp',
])

/**
 * Ask the homeserver, through Axon, for one link's preview (§ 2). Its
 * thumbnail is read back at once and kept as bytes; the staged upload is
 * left to expire (§ 7). A thumbnail that cannot be read costs the card its
 * image, never its text, and the page is never asked for again to recover it.
 */
export function homeserverLinkPreview(
  api: ApiClient,
  accountId: string,
): (url: string, signal: AbortSignal) => Promise<PreviewResult> {
  return async (url, signal) => {
    const fail = (kind: HomeserverFailure) =>
      new PreviewLookupError({ source: 'homeserver', kind })
    let generated
    try {
      ;({ data: generated } = await api.POST(
        '/v1/accounts/{account_id}/link-previews/generate',
        {
          params: { path: { account_id: accountId } },
          body: { url },
          signal,
        },
      ))
    } catch (error) {
      if (signal.aborted) throw error
      throw fail('unavailable')
    }
    const answer = generated?.data
    if (answer === undefined) throw fail('unavailable')
    if (answer.status !== 'ready') throw fail(answer.status)
    if (answer.preview == null) throw fail('no_metadata')
    let image: PreviewThumbnail | null = null
    let imageLost = answer.thumbnail_status === 'unavailable'
    const uploadId = answer.preview.image_upload_id
    if (uploadId != null) {
      image = await readThumbnail(api, accountId, uploadId, signal)
      imageLost = image === null
    }
    const result = normalized(
      {
        title: answer.preview.title,
        description: answer.preview.description,
        siteName: answer.preview.site_name,
      },
      image,
      imageLost,
    )
    if (result === null) throw fail('no_metadata')
    return result
  }
}

async function readThumbnail(
  api: ApiClient,
  accountId: string,
  uploadId: string,
  signal: AbortSignal,
): Promise<PreviewThumbnail | null> {
  try {
    const { data } = await api.GET(
      '/v1/accounts/{account_id}/media/uploads/{upload_id}/preview',
      {
        params: { path: { account_id: accountId, upload_id: uploadId } },
        parseAs: 'blob',
        signal,
      },
    )
    const blob = data as Blob | undefined
    if (
      blob === undefined ||
      blob.size === 0 ||
      blob.size > MAX_THUMBNAIL_BYTES ||
      !THUMBNAIL_TYPES.has(blob.type)
    )
      return null
    return {
      bytes: new Uint8Array(await blob.arrayBuffer()),
      mimeType: blob.type as PreviewThumbnail['mimeType'],
    }
  } catch (error) {
    if (signal.aborted) throw error
    return null
  }
}

/**
 * What a card says when it has no preview (ADR 0132; issue 1059).
 *
 * `public_ipv4_required` covers two causes the resolvers cannot tell apart:
 * a site with only IPv6 addresses, and a name that does not exist, such as
 * a mistyped one. Both have no IPv4 address, so the copy says that, and
 * names both causes.
 */
export function previewFailureMessage(failure: PreviewFailure): string {
  if (failure.source === 'device')
    switch (failure.kind) {
      case 'unsupported_url':
        return "This link can't be previewed on this device."
      case 'public_ipv4_required':
        return 'No IPv4 address was found for this site. Check the link; on-device previews also cannot reach sites that only have IPv6 addresses.'
      case 'timeout':
        return 'The site took too long to answer.'
      case 'no_metadata':
        return 'The page has no preview.'
      case 'busy':
        return 'Too many previews are loading at once.'
      case 'unavailable':
        return "Couldn't build a preview on this device."
    }
  switch (failure.kind) {
    case 'unsupported':
      return "Your homeserver can't preview this link."
    case 'disabled':
      return "Your homeserver doesn't offer link previews."
    case 'timeout':
      return 'Your homeserver took too long to answer.'
    case 'busy':
      return 'Your homeserver is busy. Try again in a moment.'
    case 'no_metadata':
      return 'Your homeserver found no preview for this page.'
    case 'unavailable':
      return "Your homeserver couldn't build a preview."
  }
}
