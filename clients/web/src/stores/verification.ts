import { computed, signal, type ReadonlySignal } from '@preact/signals'
import { apiErrorMessage, type ApiClient } from '../api/client'
import type {
  VerificationFrameKind,
  VerificationFramePayload,
} from '../api/frames'
import type { components } from '../api/schema'

export type FlowStage = components['schemas']['FlowStageDto']
export type EmojiDto = components['schemas']['EmojiDto']
export type FlowDto = components['schemas']['FlowDto']
export type DeviceDto = components['schemas']['DeviceDto']

export type VerificationDirection = 'incoming' | 'outgoing' | 'unknown'

/** UI stage; distinct from server FlowStage (mirrors TUI VerificationStage). */
export type VerificationUiStage =
  'starting' | 'waiting' | 'compare' | 'confirming' | 'done' | 'ended'

export interface VerificationFlow {
  accountId: string
  flowId: string | null
  userId: string
  /** Null on cross-user inbound (wire `device_id`). Never coerce to ''. */
  deviceId: string | null
  direction: VerificationDirection
  stage: VerificationUiStage
  serverStage: FlowStage | null
  emoji: EmojiDto[] | null
  decimals: [number, number, number] | null
  cancelReason: string | null
  error: string | null
  /** True when userId is non-empty and !== the account's own user_id. */
  crossUser: boolean
  /** Cancel clicked (or Decline) before flowId existed; send cancel on bind. */
  cancelRequested: boolean
}

export type VerifyMutationResult = { ok: true } | { ok: false; message: string }

export type VerifyStartResult =
  { ok: true; key: string } | { ok: false; message: string }

/** 404 / omitted-from-list copy, matching ADR 0028 §3 / TUI. */
export const VERIFICATION_ENDED_BY_SERVER =
  'Verification ended — the flow was cancelled by the server'

export const INCOMPLETE_EMOJI_MESSAGE = 'Waiting for a complete emoji set'

const CAP = {
  symbol: 32,
  description: 64,
  flowId: 128,
  deviceId: 64,
  userId: 255,
} as const

const FLOW_STAGES: ReadonlySet<string> = new Set([
  'requested',
  'ready',
  'keys_exchanged',
  'confirmed',
  'done',
  'cancelled',
])

export function flowKey(
  flow: Pick<VerificationFlow, 'accountId' | 'flowId' | 'deviceId'>,
): string {
  if (flow.flowId !== null && flow.flowId !== '') {
    return `${flow.accountId}\0${flow.flowId}`
  }
  return `${flow.accountId}\0pending:${flow.deviceId ?? ''}`
}

/**
 * Whether the attention surfaces (band, chip, panel, account resume) show this
 * flow. Pure. Unit-test this; do not re-derive in three components.
 *
 * Live stages always show.
 * Local cancel / Decline: false (dropLocal).
 * Remote `cancelled` / GET omission / 404 while parked: false (auto-drop).
 * Remote `done` while parked (modal not open): true, as a dismiss-only row.
 */
export function flowTitle(flow: VerificationFlow): string {
  const target = flow.deviceId ?? flow.userId
  if (flow.crossUser) {
    return `Verify ${flow.userId}`
  }
  if (flow.direction === 'outgoing') {
    return `Verifying ${target}`
  }
  if (flow.direction === 'incoming') {
    return `Verify ${target}`
  }
  return `Verification with ${target}`
}

export function flowStageLabel(flow: VerificationFlow): string {
  switch (flow.stage) {
    case 'starting':
    case 'waiting':
      return 'Waiting'
    case 'compare':
      return 'Compare emoji'
    case 'confirming':
      return 'Confirming'
    case 'done':
      return 'Complete'
    case 'ended':
      return 'Ended'
  }
}

export function inboxVisible(flow: VerificationFlow): boolean {
  if (flow.cancelRequested && flow.stage !== 'done') {
    return false
  }
  return (
    flow.stage === 'starting' ||
    flow.stage === 'waiting' ||
    flow.stage === 'compare' ||
    flow.stage === 'confirming' ||
    flow.stage === 'done'
  )
}

export type DevicePickerTarget = {
  accountId: string
  ownDeviceId: string | null
}

export interface VerificationStore {
  flows: ReadonlySignal<VerificationFlow[]>
  inbox: ReadonlySignal<VerificationFlow[]>
  inboxCount: ReadonlySignal<number>
  openKey: ReadonlySignal<string | null>
  openFlow: ReadonlySignal<VerificationFlow | null>
  /** Shell-owned device picker. Exclusive with `openFlow` (WCR-14). */
  picker: ReadonlySignal<DevicePickerTarget | null>
  /** Keyed by accountId. Never a single global list. */
  devicesByAccount: ReadonlySignal<Readonly<Record<string, DeviceDto[]>>>
  devicesLoading: ReadonlySignal<Readonly<Record<string, boolean>>>
  devicesError: ReadonlySignal<Readonly<Record<string, string | null>>>

  refreshAll(accountIds: readonly string[]): Promise<void>
  refresh(accountId: string): Promise<void>
  ensureLoaded(accountIds: readonly string[]): Promise<void>
  resetSession(): void

  loadDevices(accountId: string): Promise<void>
  start(accountId: string, deviceId: string): Promise<VerifyStartResult>
  confirm(accountId: string, flowId: string): Promise<VerifyMutationResult>
  cancel(accountId: string, flowId: string): Promise<VerifyMutationResult>
  /** Abort a starting flow whose flowId may not exist yet. */
  requestCancel(key: string): Promise<void>

  open(key: string): void
  openFlowRecord(flow: VerificationFlow): void
  closeModal(): void
  openPicker(target: DevicePickerTarget): void
  closePicker(): void
  dismissTerminal(key: string): void
  noteFrame(
    accountId: string,
    kind: VerificationFrameKind,
    payload: VerificationFramePayload,
  ): void
  applyOwnUserMap(
    accounts: ReadonlyArray<{ account_id: string; user_id: string }>,
  ): void
}

function cap(value: string, max: number): string {
  return value.length <= max ? value : value.slice(0, max)
}

function capUserId(value: string): string {
  return cap(value, CAP.userId)
}

function capDeviceId(value: string | null): string | null {
  return value === null ? null : cap(value, CAP.deviceId)
}

function capFlowId(value: string): string {
  return cap(value, CAP.flowId)
}

function sanitizeEmoji(
  raw: readonly EmojiDto[] | null | undefined,
): EmojiDto[] | null {
  if (raw === null || raw === undefined) {
    return null
  }
  const emoji: EmojiDto[] = []
  for (const item of raw) {
    if (emoji.length >= 7) {
      break
    }
    if (
      typeof item.symbol !== 'string' ||
      typeof item.description !== 'string'
    ) {
      continue
    }
    emoji.push({
      symbol: cap(item.symbol, CAP.symbol),
      description: cap(item.description, CAP.description),
    })
  }
  return emoji.length === 0 ? null : emoji
}

function sanitizeDecimals(
  raw: readonly number[] | null | undefined,
): [number, number, number] | null {
  if (raw === null || raw === undefined || raw.length !== 3) {
    return null
  }
  if (
    !raw.every(
      (entry) => typeof entry === 'number' && Number.isSafeInteger(entry),
    )
  ) {
    return null
  }
  return [raw[0], raw[1], raw[2]]
}

function parseStage(value: string | null | undefined): FlowStage | null {
  if (value === undefined || value === null) {
    return null
  }
  return FLOW_STAGES.has(value) ? (value as FlowStage) : null
}

/**
 * Live UI progress. A stale GET or replayed frame may only move this number
 * forward. `done` and `ended` are absorbing: they do not convert into each
 * other either (a late `cancelled` snapshot must not un-verify).
 */
function uiRank(stage: VerificationUiStage): number {
  switch (stage) {
    case 'starting':
    case 'waiting':
      return 0
    case 'compare':
      return 1
    case 'confirming':
      return 2
    case 'done':
    case 'ended':
      return 3
  }
}

function mapServerStage(
  serverStage: FlowStage | null,
  emoji: EmojiDto[] | null,
  cancelReason: string | null,
): {
  stage: VerificationUiStage
  error: string | null
  cancelReason: string | null
} {
  if (serverStage === 'requested' || serverStage === 'ready') {
    return { stage: 'waiting', error: null, cancelReason }
  }
  if (serverStage === 'keys_exchanged' || serverStage === null) {
    if (emoji !== null && emoji.length === 7) {
      return { stage: 'compare', error: null, cancelReason }
    }
    return {
      stage: 'waiting',
      error:
        emoji !== null && emoji.length > 0 ? INCOMPLETE_EMOJI_MESSAGE : null,
      cancelReason,
    }
  }
  if (serverStage === 'confirmed') {
    return { stage: 'confirming', error: null, cancelReason }
  }
  if (serverStage === 'done') {
    return { stage: 'done', error: null, cancelReason }
  }
  if (serverStage === 'cancelled') {
    return {
      stage: 'ended',
      error: null,
      cancelReason:
        cancelReason !== null && cancelReason !== ''
          ? cancelReason
          : 'Verification cancelled',
    }
  }
  if (emoji !== null && emoji.length === 7) {
    return { stage: 'compare', error: null, cancelReason }
  }
  return { stage: 'waiting', error: null, cancelReason }
}

function stageFrom(
  serverStage: FlowStage | null,
  emoji: EmojiDto[] | null,
  cancelReason: string | null,
  previous: VerificationUiStage,
): {
  stage: VerificationUiStage
  error: string | null
  cancelReason: string | null
} {
  const mapped = mapServerStage(serverStage, emoji, cancelReason)
  // Completing (or cancelling) is terminal for this flow id. A GET that was
  // in flight before the write committed still carries the pre-terminal
  // snapshot; holding here closes that race for every remaining previous
  // stage, not only compare / confirming.
  if (previous === 'done' || previous === 'ended') {
    return {
      stage: previous,
      error: null,
      cancelReason: previous === 'ended' ? mapped.cancelReason : cancelReason,
    }
  }
  if (uiRank(mapped.stage) < uiRank(previous)) {
    return { stage: previous, error: null, cancelReason }
  }
  return mapped
}

function isPendingOutgoingDevice(
  flow: VerificationFlow,
  accountId: string,
  deviceId: string | null,
): boolean {
  return (
    flow.accountId === accountId &&
    flow.flowId === null &&
    flow.direction === 'outgoing' &&
    deviceId !== null &&
    deviceId !== '' &&
    flow.deviceId === deviceId
  )
}

function crossUserOf(
  userId: string,
  accountId: string,
  ownUserMap: ReadonlyMap<string, string>,
): boolean {
  if (userId === '') {
    return false
  }
  const own = ownUserMap.get(accountId)
  return own !== undefined && own !== '' && userId !== own
}

/**
 * SAS verification store (ADR 0027/0028 web consumer).
 *
 * Flows are ephemeral and in-memory only. GET is the source of truth; WS
 * frames overlay via `liveAdds` the same way `invites.ts` keeps rows that
 * arrived while a list GET was in flight.
 */
export function createVerificationStore(api: ApiClient): VerificationStore {
  const flowMap = new Map<string, VerificationFlow>()
  const flows = signal<VerificationFlow[]>([])
  const openKey = signal<string | null>(null)
  const picker = signal<DevicePickerTarget | null>(null)
  const openFlow = computed((): VerificationFlow | null => {
    const key = openKey.value
    if (key === null) {
      return null
    }
    return flows.value.find((flow) => flowKey(flow) === key) ?? null
  })
  const inbox = computed(() => flows.value.filter((flow) => inboxVisible(flow)))
  const inboxCount = computed(() => inbox.value.length)
  const devicesByAccount = signal<Readonly<Record<string, DeviceDto[]>>>({})
  const devicesLoading = signal<Readonly<Record<string, boolean>>>({})
  const devicesError = signal<Readonly<Record<string, string | null>>>({})

  let sessionGeneration = 0
  const requestGeneration = new Map<string, number>()
  const deviceGeneration = new Map<string, number>()
  const settledAccounts = new Set<string>()
  const ownUserMap = new Map<string, string>()
  /** Per-account overlay of frames that arrived during an in-flight GET. */
  const liveAdds = new Map<string, Map<string, VerificationFlow>>()
  /**
   * Per-account flow ids removed locally (declined, dismissed, cancelled).
   * `liveAdds` keeps a GET from losing rows it never saw; this is the mirror
   * image, keeping a GET computed before the removal — or one landing inside
   * the server's terminal grace window — from resurrecting them. Retired as
   * soon as a refresh confirms the server has stopped listing the flow.
   */
  const tombstones = new Map<string, Set<string>>()

  function publish(): void {
    flows.value = [...flowMap.values()]
  }

  function put(flow: VerificationFlow, previousKey?: string): string {
    const key = flowKey(flow)
    if (previousKey !== undefined && previousKey !== key) {
      flowMap.delete(previousKey)
      if (openKey.value === previousKey) {
        openKey.value = key
      }
      for (const adds of liveAdds.values()) {
        const overlay = adds.get(previousKey)
        if (overlay !== undefined) {
          adds.delete(previousKey)
          adds.set(key, flow)
        }
      }
    }
    flowMap.set(key, flow)
    publish()
    return key
  }

  function tombstone(flow: VerificationFlow | undefined): void {
    if (flow === undefined || flow.flowId === null || flow.flowId === '') {
      return
    }
    let ids = tombstones.get(flow.accountId)
    if (ids === undefined) {
      ids = new Set<string>()
      tombstones.set(flow.accountId, ids)
    }
    ids.add(flow.flowId)
  }

  function drop(key: string): void {
    tombstone(flowMap.get(key))
    flowMap.delete(key)
    for (const adds of liveAdds.values()) {
      adds.delete(key)
    }
    if (openKey.value === key) {
      openKey.value = null
    }
    publish()
  }

  function overlayLive(accountId: string, flow: VerificationFlow): void {
    const adds = liveAdds.get(accountId)
    if (adds !== undefined) {
      adds.set(flowKey(flow), flow)
    }
  }

  function findByFlowId(
    accountId: string,
    flowId: string,
  ): VerificationFlow | undefined {
    return flowMap.get(`${accountId}\0${flowId}`)
  }

  function findPendingOutgoing(
    accountId: string,
    deviceId: string | null,
  ): VerificationFlow | undefined {
    for (const flow of flowMap.values()) {
      if (isPendingOutgoingDevice(flow, accountId, deviceId)) {
        return flow
      }
    }
    return undefined
  }

  function withIdentity(
    flow: VerificationFlow,
    userId: string,
    deviceId: string | null,
  ): VerificationFlow {
    const nextUser =
      flow.userId === '' && userId !== '' ? capUserId(userId) : flow.userId
    const nextDevice =
      flow.deviceId === null && deviceId !== null
        ? capDeviceId(deviceId)
        : flow.deviceId
    return {
      ...flow,
      userId: nextUser,
      deviceId: nextDevice,
      crossUser: crossUserOf(nextUser, flow.accountId, ownUserMap),
    }
  }

  function applyServerStage(
    flow: VerificationFlow,
    serverStage: FlowStage | null,
    emoji: EmojiDto[] | null,
    decimals: [number, number, number] | null,
    cancelReason: string | null,
  ): VerificationFlow {
    const nextEmoji = emoji ?? flow.emoji
    const nextDecimals = decimals ?? flow.decimals
    const mapped = stageFrom(
      serverStage,
      nextEmoji,
      cancelReason ?? flow.cancelReason,
      flow.stage,
    )
    // `stageFrom` re-derives the incomplete-emoji warning from the snapshot in
    // hand, so carrying the old one forward would latch it: the full seven
    // emoji would arrive and render the compare UI under a stale red alert.
    // Mutation errors (a 409 from confirm, and so on) stay only while the UI
    // stage does not change; a later legitimate advance must not keep the
    // stale banner.
    const carried =
      mapped.stage !== flow.stage || flow.error === INCOMPLETE_EMOJI_MESSAGE
        ? null
        : flow.error
    return {
      ...flow,
      serverStage: serverStage ?? flow.serverStage,
      emoji: nextEmoji,
      decimals: nextDecimals,
      stage: mapped.stage,
      error: mapped.error ?? carried,
      cancelReason: mapped.cancelReason,
    }
  }

  function bindFlowId(
    flow: VerificationFlow,
    flowId: string,
  ): VerificationFlow {
    if (flow.flowId !== null && flow.flowId !== '') {
      return flow
    }
    return {
      ...flow,
      flowId: capFlowId(flowId),
      stage: flow.stage === 'starting' ? 'waiting' : flow.stage,
    }
  }

  async function postCancel(
    accountId: string,
    flowId: string,
  ): Promise<VerifyMutationResult> {
    try {
      const { error: apiError, response } = await api.POST(
        '/v1/accounts/{account_id}/verify/{flow_id}/cancel',
        {
          params: { path: { account_id: accountId, flow_id: flowId } },
        },
      )
      if (apiError !== undefined || !response.ok) {
        return {
          ok: false,
          message:
            apiError !== undefined
              ? apiErrorMessage(apiError)
              : 'unexpected server response',
        }
      }
      return { ok: true }
    } catch (cause) {
      return {
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      }
    }
  }

  async function cancelAfterBind(flow: VerificationFlow): Promise<void> {
    if (flow.flowId === null || flow.flowId === '') {
      return
    }
    const result = await postCancel(flow.accountId, flow.flowId)
    const key = flowKey(flow)
    const current = flowMap.get(key)
    if (current === undefined) {
      return
    }
    if (result.ok) {
      drop(key)
      return
    }
    put({ ...current, error: result.message, cancelRequested: true })
  }

  function implicitCancel(flow: VerificationFlow): void {
    // A flow that already succeeded is not a cancel just because the server
    // has since forgotten it: completed flows stay as dismiss-only rows, and
    // an open modal must keep saying the verification completed.
    if (flow.stage === 'done' || flow.serverStage === 'done') {
      return
    }
    const key = flowKey(flow)
    if (openKey.value === key) {
      put({
        ...flow,
        stage: 'ended',
        cancelReason: VERIFICATION_ENDED_BY_SERVER,
        error: null,
      })
      return
    }
    drop(key)
  }

  function fromDto(
    accountId: string,
    dto: FlowDto,
    direction: VerificationDirection,
    previous?: VerificationFlow,
  ): VerificationFlow {
    const emoji = sanitizeEmoji(dto.emoji ?? null)
    const decimals = sanitizeDecimals(dto.decimals ?? null)
    const deviceId = capDeviceId(dto.device_id ?? null)
    const userId = capUserId(dto.user_id)
    const flowId = capFlowId(dto.flow_id)
    const serverStage = parseStage(dto.stage)
    const base: VerificationFlow = previous ?? {
      accountId,
      flowId,
      userId,
      deviceId,
      direction,
      stage: 'waiting',
      serverStage,
      emoji: null,
      decimals: null,
      cancelReason: null,
      error: null,
      crossUser: crossUserOf(userId, accountId, ownUserMap),
      cancelRequested: false,
    }
    const identified = withIdentity(
      { ...base, flowId, direction: previous?.direction ?? direction },
      userId,
      deviceId,
    )
    return applyServerStage(
      identified,
      serverStage,
      emoji,
      decimals,
      dto.cancel_reason ?? null,
    )
  }

  function resetSession(): void {
    sessionGeneration += 1
    // `devicesLoading`/`devicesError` have to count towards "nothing to
    // clear": bumping the generation orphans an in-flight `loadDevices`, whose
    // `finally` then declines to reset its own loading flag. Taking the fast
    // path on those would strand DevicePicker on "Loading devices…" with Retry
    // disabled for the rest of the session.
    const devicesEmpty =
      Object.keys(devicesByAccount.value).length === 0 &&
      Object.keys(devicesLoading.value).length === 0 &&
      Object.keys(devicesError.value).length === 0
    if (
      flowMap.size === 0 &&
      openKey.value === null &&
      picker.value === null &&
      devicesEmpty
    ) {
      settledAccounts.clear()
      liveAdds.clear()
      tombstones.clear()
      requestGeneration.clear()
      deviceGeneration.clear()
      ownUserMap.clear()
      return
    }
    flowMap.clear()
    liveAdds.clear()
    tombstones.clear()
    settledAccounts.clear()
    requestGeneration.clear()
    deviceGeneration.clear()
    ownUserMap.clear()
    openKey.value = null
    picker.value = null
    devicesByAccount.value = {}
    devicesLoading.value = {}
    devicesError.value = {}
    publish()
  }

  async function refresh(accountId: string): Promise<void> {
    const generation = sessionGeneration
    const reqGen = (requestGeneration.get(accountId) ?? 0) + 1
    requestGeneration.set(accountId, reqGen)
    const added = new Map<string, VerificationFlow>()
    liveAdds.set(accountId, added)
    try {
      const { data, error: apiError } = await api.GET(
        '/v1/accounts/{account_id}/verify',
        { params: { path: { account_id: accountId } } },
      )
      if (generation !== sessionGeneration) {
        return
      }
      if (requestGeneration.get(accountId) !== reqGen) {
        return
      }
      if (apiError !== undefined || data === undefined) {
        return
      }
      const serverRows = data.data
      const serverIds = new Set(serverRows.map((row) => capFlowId(row.flow_id)))
      // Retire before suppressing: once the server has stopped listing a flow
      // there is nothing left to resurrect, and holding the id forever would
      // grow the set for the life of the session.
      const buried = tombstones.get(accountId)
      if (buried !== undefined) {
        for (const flowId of [...buried]) {
          if (!serverIds.has(flowId)) {
            buried.delete(flowId)
          }
        }
        if (buried.size === 0) {
          tombstones.delete(accountId)
        }
      }
      const existing = [...flowMap.values()].filter(
        (flow) => flow.accountId === accountId,
      )
      const seen = new Set<string>()
      for (const dto of serverRows) {
        const flowId = capFlowId(dto.flow_id)
        if (buried?.has(flowId) === true) {
          continue
        }
        const previous =
          findByFlowId(accountId, flowId) ??
          existing.find(
            (flow) =>
              flow.flowId === null &&
              flow.direction === 'outgoing' &&
              (dto.device_id ?? null) !== null &&
              flow.deviceId === dto.device_id,
          )
        const merged = fromDto(
          accountId,
          dto,
          previous?.direction ?? 'unknown',
          previous,
        )
        const previousKey =
          previous !== undefined ? flowKey(previous) : undefined
        put(merged, previousKey)
        seen.add(flowKey(merged))
      }
      for (const flow of existing) {
        const key = flowKey(flow)
        if (seen.has(key)) {
          continue
        }
        if (flow.flowId === null) {
          continue
        }
        if (added.has(key)) {
          continue
        }
        if (serverIds.has(flow.flowId)) {
          continue
        }
        if (flow.cancelRequested) {
          drop(key)
          continue
        }
        implicitCancel(flowMap.get(key) ?? flow)
      }
      for (const [key, overlay] of added) {
        if (buried?.has(overlay.flowId ?? '') === true) {
          continue
        }
        if (!flowMap.has(key) && !serverIds.has(overlay.flowId ?? '')) {
          put(overlay)
        }
      }
      settledAccounts.add(accountId)
    } catch {
      if (generation !== sessionGeneration) {
        return
      }
    }
  }

  async function refreshAll(accountIds: readonly string[]): Promise<void> {
    await Promise.allSettled(accountIds.map((id) => refresh(id)))
  }

  async function ensureLoaded(accountIds: readonly string[]): Promise<void> {
    const missing = accountIds.filter((id) => !settledAccounts.has(id))
    if (missing.length === 0) {
      return
    }
    await refreshAll(missing)
  }

  async function loadDevices(accountId: string): Promise<void> {
    const generation = sessionGeneration
    const reqGen = (deviceGeneration.get(accountId) ?? 0) + 1
    deviceGeneration.set(accountId, reqGen)
    devicesLoading.value = { ...devicesLoading.value, [accountId]: true }
    devicesError.value = { ...devicesError.value, [accountId]: null }
    try {
      const { data, error: apiError } = await api.GET(
        '/v1/accounts/{account_id}/devices',
        { params: { path: { account_id: accountId } } },
      )
      if (
        generation !== sessionGeneration ||
        deviceGeneration.get(accountId) !== reqGen
      ) {
        return
      }
      if (apiError !== undefined || data === undefined) {
        devicesError.value = {
          ...devicesError.value,
          [accountId]:
            apiError === undefined
              ? 'device list failed'
              : apiErrorMessage(apiError),
        }
        return
      }
      devicesByAccount.value = {
        ...devicesByAccount.value,
        [accountId]: data.data.devices,
      }
    } catch (cause) {
      if (
        generation !== sessionGeneration ||
        deviceGeneration.get(accountId) !== reqGen
      ) {
        return
      }
      devicesError.value = {
        ...devicesError.value,
        [accountId]: cause instanceof Error ? cause.message : String(cause),
      }
    } finally {
      if (
        generation === sessionGeneration &&
        deviceGeneration.get(accountId) === reqGen
      ) {
        devicesLoading.value = { ...devicesLoading.value, [accountId]: false }
      }
    }
  }

  async function start(
    accountId: string,
    deviceId: string,
  ): Promise<VerifyStartResult> {
    const generation = sessionGeneration
    const pending: VerificationFlow = {
      accountId,
      flowId: null,
      userId: ownUserMap.get(accountId) ?? '',
      deviceId: capDeviceId(deviceId),
      direction: 'outgoing',
      stage: 'starting',
      serverStage: null,
      emoji: null,
      decimals: null,
      cancelReason: null,
      error: null,
      crossUser: false,
      cancelRequested: false,
    }
    const pendingKey = put(pending)
    try {
      const { data, error: apiError } = await api.POST(
        '/v1/accounts/{account_id}/verify',
        {
          params: { path: { account_id: accountId } },
          body: { device_id: deviceId },
        },
      )
      if (generation !== sessionGeneration) {
        return { ok: false, message: 'session ended' }
      }
      if (apiError !== undefined || data === undefined) {
        drop(pendingKey)
        return {
          ok: false,
          message:
            apiError === undefined
              ? 'unexpected server response'
              : apiErrorMessage(apiError),
        }
      }
      const flowId = capFlowId(data.data.flow_id)
      const current =
        flowMap.get(pendingKey) ??
        findByFlowId(accountId, flowId) ??
        findPendingOutgoing(accountId, pending.deviceId)
      if (current === undefined) {
        return { ok: true, key: `${accountId}\0${flowId}` }
      }
      const bound = bindFlowId(
        withIdentity(current, current.userId, current.deviceId),
        flowId,
      )
      const key = put(bound, flowKey(current))
      if (bound.cancelRequested) {
        await cancelAfterBind(flowMap.get(key) ?? bound)
        return { ok: true, key }
      }
      return { ok: true, key }
    } catch (cause) {
      if (generation === sessionGeneration) {
        drop(pendingKey)
      }
      return {
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      }
    }
  }

  async function confirm(
    accountId: string,
    flowId: string,
  ): Promise<VerifyMutationResult> {
    const key = `${accountId}\0${flowId}`
    const flow = flowMap.get(key)
    if (flow === undefined) {
      return { ok: false, message: 'No such verification flow' }
    }
    if (flow.stage === 'confirming' || flow.stage === 'done') {
      return { ok: true }
    }
    if (
      flow.stage !== 'compare' ||
      flow.emoji === null ||
      flow.emoji.length !== 7
    ) {
      return { ok: false, message: INCOMPLETE_EMOJI_MESSAGE }
    }
    put({ ...flow, stage: 'confirming', error: null })
    try {
      const { error: apiError, response } = await api.POST(
        '/v1/accounts/{account_id}/verify/{flow_id}/confirm',
        {
          params: { path: { account_id: accountId, flow_id: flowId } },
        },
      )
      const current = flowMap.get(key)
      if (current === undefined) {
        return { ok: false, message: 'No such verification flow' }
      }
      if (current.stage === 'done' || current.stage === 'ended') {
        if (apiError !== undefined || !response.ok) {
          return {
            ok: false,
            message:
              apiError !== undefined
                ? apiErrorMessage(apiError)
                : 'unexpected server response',
          }
        }
        return { ok: true }
      }
      if (apiError !== undefined || !response.ok) {
        const message =
          apiError !== undefined
            ? apiErrorMessage(apiError)
            : 'unexpected server response'
        if (current.stage === 'confirming') {
          put({ ...current, error: message, stage: 'compare' })
        }
        return { ok: false, message }
      }
      put({
        ...current,
        stage: 'confirming',
        error: null,
        serverStage: 'confirmed',
      })
      return { ok: true }
    } catch (cause) {
      const current = flowMap.get(key)
      const message = cause instanceof Error ? cause.message : String(cause)
      if (current !== undefined && current.stage === 'confirming') {
        put({ ...current, error: message, stage: 'compare' })
      }
      return { ok: false, message }
    }
  }

  async function cancel(
    accountId: string,
    flowId: string,
  ): Promise<VerifyMutationResult> {
    return requestCancel(`${accountId}\0${flowId}`).then(() => {
      const flow = flowMap.get(`${accountId}\0${flowId}`)
      if (flow === undefined) {
        return { ok: true }
      }
      return flow.error !== null
        ? { ok: false, message: flow.error }
        : { ok: true }
    })
  }

  async function requestCancel(key: string): Promise<void> {
    const flow = flowMap.get(key)
    if (flow === undefined) {
      return
    }
    const marked: VerificationFlow = {
      ...flow,
      cancelRequested: true,
    }
    put(marked)
    if (marked.flowId === null || marked.flowId === '') {
      return
    }
    await cancelAfterBind(marked)
  }

  function noteFrame(
    accountId: string,
    kind: VerificationFrameKind,
    payload: VerificationFramePayload,
  ): void {
    const flowId = capFlowId(payload.flowId)
    const userId = capUserId(payload.userId)
    const deviceId = capDeviceId(payload.deviceId)
    const emoji = sanitizeEmoji(payload.emoji)
    const decimals = sanitizeDecimals(payload.decimals)

    let flow =
      findByFlowId(accountId, flowId) ??
      (kind === 'requested' ||
      kind === 'sas' ||
      kind === 'done' ||
      kind === 'cancelled'
        ? findPendingOutgoing(accountId, deviceId)
        : undefined)

    if (flow === undefined) {
      // Same invariant as the GET path: a flow the user removed here does not
      // come back because a frame for it was already in flight.
      if (tombstones.get(accountId)?.has(flowId) === true) {
        return
      }
      if (kind === 'requested' || kind === 'sas') {
        flow = {
          accountId,
          flowId,
          userId,
          deviceId,
          direction: 'incoming',
          stage: 'waiting',
          serverStage: kind === 'sas' ? 'keys_exchanged' : 'requested',
          emoji: null,
          decimals: null,
          cancelReason: null,
          error: null,
          crossUser: crossUserOf(userId, accountId, ownUserMap),
          cancelRequested: false,
        }
      } else {
        return
      }
    } else if (flow.direction === 'outgoing' && flow.flowId === null) {
      flow = { ...flow, direction: 'outgoing' }
    }

    const previousKey = flowKey(flow)
    flow = withIdentity(bindFlowId(flow, flowId), userId, deviceId)

    // Completing and cancelling are absorbing. A redelivered `sas`/`requested`
    // or a late `done`/`cancelled` in the other direction must not rewrite a
    // terminal flow — the same no-rewind rule `stageFrom`/`uiRank` enforces on
    // GET. Skip the write entirely so `serverStage` cannot regress either.
    if (flow.stage === 'done' || flow.stage === 'ended') {
      return
    }

    if (kind === 'requested') {
      // `compare` belongs here too: a replayed request frame must not rewind a
      // flow whose emoji are already on screen, or `confirm()` rejects the
      // user's "They match" as an incomplete emoji set.
      if (!['confirming', 'compare'].includes(flow.stage)) {
        flow = { ...flow, stage: 'waiting', serverStage: 'requested' }
      }
    } else if (kind === 'sas') {
      flow = applyServerStage(flow, 'keys_exchanged', emoji, decimals, null)
    } else if (kind === 'done') {
      flow = applyServerStage(flow, 'done', emoji, decimals, null)
    } else {
      flow = applyServerStage(
        flow,
        'cancelled',
        null,
        null,
        payload.reason ?? 'Verification cancelled',
      )
    }

    if (flow.cancelRequested && kind !== 'done') {
      const key = put(flow, previousKey)
      overlayLive(accountId, flowMap.get(key) ?? flow)
      void cancelAfterBind(flowMap.get(key) ?? flow)
      return
    }

    const key = put(flow, previousKey)
    overlayLive(accountId, flowMap.get(key) ?? flow)
    if (kind === 'cancelled' && openKey.value !== key) {
      drop(key)
    }
  }

  function applyOwnUserMap(
    accounts: ReadonlyArray<{ account_id: string; user_id: string }>,
  ): void {
    ownUserMap.clear()
    for (const account of accounts) {
      ownUserMap.set(account.account_id, account.user_id)
    }
    for (const flow of [...flowMap.values()]) {
      const next = {
        ...flow,
        crossUser: crossUserOf(flow.userId, flow.accountId, ownUserMap),
      }
      if (next.crossUser !== flow.crossUser) {
        put(next)
      }
    }
  }

  return {
    flows,
    inbox,
    inboxCount,
    openKey,
    openFlow,
    picker,
    devicesByAccount,
    devicesLoading,
    devicesError,
    refreshAll,
    refresh,
    ensureLoaded,
    resetSession,
    loadDevices,
    start,
    confirm,
    cancel,
    requestCancel,
    open: (key) => {
      if (flowMap.has(key)) {
        picker.value = null
        openKey.value = key
      }
    },
    openFlowRecord: (flow) => {
      picker.value = null
      openKey.value = put(flow)
    },
    closeModal: () => {
      openKey.value = null
    },
    openPicker: (target) => {
      openKey.value = null
      picker.value = {
        accountId: target.accountId,
        ownDeviceId: target.ownDeviceId,
      }
    },
    closePicker: () => {
      picker.value = null
    },
    dismissTerminal: (key) => {
      drop(key)
    },
    noteFrame,
    applyOwnUserMap,
  }
}
