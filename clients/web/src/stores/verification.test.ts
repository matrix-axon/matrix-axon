import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import type { VerificationFramePayload } from '../api/frames'
import {
  createVerificationStore,
  flowKey,
  inboxVisible,
  INCOMPLETE_EMOJI_MESSAGE,
  VERIFICATION_ENDED_BY_SERVER,
  type DeviceDto,
  type EmojiDto,
  type FlowDto,
  type VerificationFlow,
} from './verification'

const BASE = 'http://axon.test'
const ACCOUNT = '11111111-1111-1111-1111-111111111111'
const ACCOUNT_B = '22222222-2222-2222-2222-222222222222'
const ME = '@me:hs'
const DEVICE = 'AXONDEVICE'
const SIBLING = 'ELEMENT'

function store() {
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE,
  )
  const verification = createVerificationStore(api)
  verification.applyOwnUserMap([{ account_id: ACCOUNT, user_id: ME }])
  return verification
}

function sevenEmoji(): EmojiDto[] {
  return [
    { symbol: '🐶', description: 'Dog' },
    { symbol: '🐱', description: 'Cat' },
    { symbol: '🦁', description: 'Lion' },
    { symbol: '🐴', description: 'Horse' },
    { symbol: '🦄', description: 'Unicorn' },
    { symbol: '🐷', description: 'Pig' },
    { symbol: '🐘', description: 'Elephant' },
  ]
}

function flowDto(overrides: Partial<FlowDto> = {}): FlowDto {
  return {
    flow_id: '$flow',
    user_id: ME,
    device_id: SIBLING,
    stage: 'requested',
    emoji: null,
    decimals: null,
    cancel_reason: null,
    ...overrides,
  }
}

function payload(
  overrides: Partial<VerificationFramePayload> = {},
): VerificationFramePayload {
  return {
    flowId: '$flow',
    userId: ME,
    deviceId: SIBLING,
    emoji: null,
    decimals: null,
    reason: null,
    ...overrides,
  }
}

function device(id: string, verified = true): DeviceDto {
  return {
    device_id: id,
    display_name: id,
    algorithms: ['m.megolm.v1.aes-sha2'],
    is_verified: verified,
    is_cross_signed_by_owner: verified,
    local_trust_state: verified ? 'verified' : 'unset',
  }
}

describe('inboxVisible', () => {
  const base: VerificationFlow = {
    accountId: ACCOUNT,
    flowId: '$flow',
    userId: ME,
    deviceId: SIBLING,
    direction: 'incoming',
    stage: 'waiting',
    serverStage: 'requested',
    emoji: null,
    decimals: null,
    cancelReason: null,
    error: null,
    crossUser: false,
    cancelRequested: false,
  }

  it('shows live stages and parked done; hides local cancel and remote ended', () => {
    expect(inboxVisible({ ...base, stage: 'starting' })).toBe(true)
    expect(inboxVisible({ ...base, stage: 'waiting' })).toBe(true)
    expect(inboxVisible({ ...base, stage: 'compare' })).toBe(true)
    expect(inboxVisible({ ...base, stage: 'confirming' })).toBe(true)
    expect(inboxVisible({ ...base, stage: 'done' })).toBe(true)
    expect(inboxVisible({ ...base, stage: 'ended' })).toBe(false)
    expect(
      inboxVisible({ ...base, cancelRequested: true, stage: 'waiting' }),
    ).toBe(false)
  })
})

describe('verification store', () => {
  const server = setupServer()
  beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
  afterEach(() => server.resetHandlers())
  afterAll(() => server.close())

  it('start POSTs device_id only, maps flow_id, and rekeys pending', async () => {
    const bodies: unknown[] = []
    server.use(
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/verify`,
        async ({ request }) => {
          bodies.push(await request.json())
          return HttpResponse.json({ data: { flow_id: '$flow' } })
        },
      ),
    )
    const verification = store()
    const result = await verification.start(ACCOUNT, SIBLING)
    expect(bodies).toEqual([{ device_id: SIBLING }])
    expect(result).toEqual({ ok: true, key: `${ACCOUNT}\0$flow` })
    const flow = verification.flows.value[0]
    expect(flow.flowId).toBe('$flow')
    expect(flow.direction).toBe('outgoing')
    expect(flow.stage).toBe('waiting')
    expect(flowKey(flow)).toBe(`${ACCOUNT}\0$flow`)
  })

  it('pending outgoing + requested echo binds flow_id and does not duplicate', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify`, async () => {
        await held
        return HttpResponse.json({ data: { flow_id: '$flow' } })
      }),
    )
    const verification = store()
    const pending = verification.start(ACCOUNT, SIBLING)
    verification.noteFrame(ACCOUNT, 'requested', payload())
    expect(verification.flows.value).toHaveLength(1)
    expect(verification.flows.value[0].flowId).toBe('$flow')
    expect(verification.flows.value[0].direction).toBe('outgoing')
    release?.()
    await pending
    expect(verification.flows.value).toHaveLength(1)
    expect(verification.inbox.value).toHaveLength(1)
  })

  it('concurrent inbound requested for a different own device creates a second flow', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify`, async () => {
        await held
        return HttpResponse.json({ data: { flow_id: '$out' } })
      }),
    )
    const verification = store()
    const pending = verification.start(ACCOUNT, SIBLING)
    verification.noteFrame(
      ACCOUNT,
      'requested',
      payload({ flowId: '$in', deviceId: 'PHONE' }),
    )
    expect(verification.flows.value).toHaveLength(2)
    expect(
      verification.flows.value.some(
        (flow) => flow.direction === 'incoming' && flow.deviceId === 'PHONE',
      ),
    ).toBe(true)
    expect(
      verification.flows.value.some(
        (flow) => flow.direction === 'outgoing' && flow.deviceId === SIBLING,
      ),
    ).toBe(true)
    release?.()
    await pending
    expect(verification.flows.value).toHaveLength(2)
  })

  it('sas with 7 emoji reaches compare; 1 emoji does not enable confirm', async () => {
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(
      ACCOUNT,
      'sas',
      payload({ emoji: [{ symbol: '🐶', description: 'Dog' }] }),
    )
    expect(verification.flows.value[0].stage).toBe('waiting')
    expect(verification.flows.value[0].error).toBe(INCOMPLETE_EMOJI_MESSAGE)
    const confirmShort = await verification.confirm(ACCOUNT, '$flow')
    expect(confirmShort.ok).toBe(false)

    verification.noteFrame(
      ACCOUNT,
      'sas',
      payload({ emoji: sevenEmoji(), decimals: [1, 2, 3] }),
    )
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].emoji).toHaveLength(7)
    expect(verification.flows.value[0].decimals).toEqual([1, 2, 3])
  })

  it('confirm 204 becomes confirming and a late sas does not regress', async () => {
    server.use(
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/confirm`,
        () => new HttpResponse(null, { status: 204 }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    const result = await verification.confirm(ACCOUNT, '$flow')
    expect(result).toEqual({ ok: true })
    expect(verification.flows.value[0].stage).toBe('confirming')
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(verification.flows.value[0].stage).toBe('confirming')
  })

  it('confirm HTTP 409 stays on compare with error set', async () => {
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/confirm`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'conflict',
              message:
                'The flow has not reached the SAS stage, or is already terminal',
            },
          },
          { status: 409 },
        ),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    const result = await verification.confirm(ACCOUNT, '$flow')
    expect(result.ok).toBe(false)
    if (result.ok) {
      return
    }
    expect(result.message).toContain('has not reached the SAS stage')
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].error).toContain(
      'has not reached the SAS stage',
    )
    expect(verification.flows.value[0].emoji).toHaveLength(7)
  })

  it('done is inbox-visible; remote cancelled while parked is not', () => {
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.open(flowKey(verification.flows.value[0]))
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.closeModal()
    verification.noteFrame(ACCOUNT, 'cancelled', payload({ reason: 'timeout' }))
    expect(verification.inbox.value).toHaveLength(0)
    expect(verification.flows.value).toEqual([])

    verification.noteFrame(ACCOUNT, 'requested', payload({ flowId: '$done' }))
    verification.noteFrame(ACCOUNT, 'done', payload({ flowId: '$done' }))
    expect(verification.inbox.value[0].stage).toBe('done')
  })

  it('requestCancel before flowId POSTs cancel once bound and never shows in inbox', async () => {
    const cancelled: string[] = []
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify`, async () => {
        await held
        return HttpResponse.json({ data: { flow_id: '$flow' } })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/cancel`,
        ({ params }) => {
          cancelled.push(String(params.flowId))
          return new HttpResponse(null, { status: 204 })
        },
      ),
    )
    const verification = store()
    const pending = verification.start(ACCOUNT, SIBLING)
    const key = verification.flows.value[0]
    expect(inboxVisible(key)).toBe(true)
    await verification.requestCancel(flowKey(key))
    expect(verification.inbox.value).toHaveLength(0)
    release?.()
    await pending
    expect(cancelled).toEqual(['$flow'])
    expect(verification.inbox.value).toHaveLength(0)
  })

  it('Escape is not requestCancel — park keeps inboxVisible', async () => {
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: { flow_id: '$flow' } }),
      ),
    )
    const verification = store()
    await verification.start(ACCOUNT, SIBLING)
    verification.open(flowKey(verification.flows.value[0]))
    verification.closeModal()
    expect(verification.openFlow.value).toBeNull()
    expect(verification.inboxCount.value).toBe(1)
  })

  it('refreshAll two accounts, one 500: the other remains and liveAdds survive', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [flowDto()] }),
      ),
      http.get(`${BASE}/v1/accounts/${ACCOUNT_B}/verify`, () =>
        HttpResponse.json(
          { error: { code: 'internal', message: 'boom' } },
          { status: 500 },
        ),
      ),
    )
    const verification = store()
    verification.noteFrame(
      ACCOUNT_B,
      'requested',
      payload({ flowId: '$b', userId: '@b:hs', deviceId: 'PHONE' }),
    )
    await verification.refreshAll([ACCOUNT, ACCOUNT_B])
    expect(
      verification.flows.value.some((flow) => flow.accountId === ACCOUNT),
    ).toBe(true)
    expect(
      verification.flows.value.some(
        (flow) => flow.accountId === ACCOUNT_B && flow.flowId === '$b',
      ),
    ).toBe(true)
  })

  it('a tracked open flow missing from GET ends with the ADR 0028 sentence', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.open(flowKey(verification.flows.value[0]))
    await verification.refresh(ACCOUNT)
    expect(verification.openFlow.value?.stage).toBe('ended')
    expect(verification.openFlow.value?.cancelReason).toBe(
      VERIFICATION_ENDED_BY_SERVER,
    )
  })

  it('keeps a live requested that arrived while GET was in flight', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, async () => {
        await held
        return HttpResponse.json({ data: [flowDto({ flow_id: '$old' })] })
      }),
    )
    const verification = store()
    const pending = verification.refresh(ACCOUNT)
    verification.noteFrame(ACCOUNT, 'requested', payload({ flowId: '$live' }))
    release?.()
    await pending
    const ids = verification.flows.value.map((flow) => flow.flowId).sort()
    expect(ids).toEqual(['$live', '$old'])
  })

  it('openPicker and open() are exclusive', () => {
    const verification = store()
    verification.openPicker({ accountId: ACCOUNT, ownDeviceId: DEVICE })
    expect(verification.picker.value).toEqual({
      accountId: ACCOUNT,
      ownDeviceId: DEVICE,
    })
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.open(`${ACCOUNT}\0$flow`)
    expect(verification.openFlow.value?.flowId).toBe('$flow')
    expect(verification.picker.value).toBeNull()
    verification.closeModal()
    verification.openPicker({ accountId: ACCOUNT, ownDeviceId: DEVICE })
    expect(verification.openKey.value).toBeNull()
    expect(verification.picker.value?.accountId).toBe(ACCOUNT)
    verification.resetSession()
    expect(verification.picker.value).toBeNull()
  })

  it('resetSession drops flows and ignores a late GET', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, async () => {
        await held
        return HttpResponse.json({ data: [flowDto()] })
      }),
    )
    const verification = store()
    const pending = verification.refresh(ACCOUNT)
    verification.resetSession()
    release?.()
    await pending
    expect(verification.flows.value).toEqual([])
  })

  it("loadDevices(A) in flight does not write B's slot", async () => {
    let releaseA: (() => void) | undefined
    const heldA = new Promise<void>((resolve) => {
      releaseA = resolve
    })
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/devices`, async () => {
        await heldA
        return HttpResponse.json({
          data: { user_id: ME, devices: [device(DEVICE), device(SIBLING)] },
        })
      }),
      http.get(`${BASE}/v1/accounts/${ACCOUNT_B}/devices`, () =>
        HttpResponse.json({
          data: {
            user_id: '@b:hs',
            devices: [device('B-DEV')],
          },
        }),
      ),
    )
    const verification = store()
    const pendingA = verification.loadDevices(ACCOUNT)
    const pendingB = verification.loadDevices(ACCOUNT_B)
    await pendingB
    expect(verification.devicesByAccount.value[ACCOUNT_B]?.[0].device_id).toBe(
      'B-DEV',
    )
    expect(verification.devicesByAccount.value[ACCOUNT]).toBeUndefined()
    releaseA?.()
    await pendingA
    expect(
      verification.devicesByAccount.value[ACCOUNT]?.map((d) => d.device_id),
    ).toEqual([DEVICE, SIBLING])
    expect(verification.devicesByAccount.value[ACCOUNT_B]?.[0].device_id).toBe(
      'B-DEV',
    )
  })

  it('GET-discovered flow with no local start is unknown; known outgoing keeps outgoing', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({
          data: [
            flowDto({
              flow_id: '$known',
              stage: 'keys_exchanged',
              emoji: sevenEmoji(),
            }),
            flowDto({ flow_id: '$new', stage: 'requested' }),
          ],
        }),
      ),
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: { flow_id: '$known' } }),
      ),
    )
    const verification = store()
    await verification.start(ACCOUNT, SIBLING)
    await verification.refresh(ACCOUNT)
    const known = verification.flows.value.find(
      (flow) => flow.flowId === '$known',
    )
    const discovered = verification.flows.value.find(
      (flow) => flow.flowId === '$new',
    )
    expect(known?.direction).toBe('outgoing')
    expect(discovered?.direction).toBe('unknown')
  })

  it('preserves a null deviceId and omits hostile decimals', () => {
    const verification = store()
    verification.noteFrame(
      ACCOUNT,
      'requested',
      payload({ userId: '@other:hs', deviceId: null, flowId: '$x' }),
    )
    verification.noteFrame(
      ACCOUNT,
      'sas',
      payload({
        flowId: '$x',
        userId: '@other:hs',
        deviceId: null,
        emoji: sevenEmoji(),
        decimals: [1, 2] as unknown as [number, number, number],
      }),
    )
    const flow = verification.flows.value[0]
    expect(flow.deviceId).toBeNull()
    expect(flow.decimals).toBeNull()
    expect(flow.crossUser).toBe(true)
    expect(flow.stage).toBe('compare')
  })

  it('a completed flow survives a GET that has forgotten it', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'done', payload())
    verification.open(flowKey(verification.flows.value[0]))
    await verification.refresh(ACCOUNT)
    expect(verification.openFlow.value?.stage).toBe('done')
    expect(verification.openFlow.value?.cancelReason).toBeNull()
    expect(verification.inbox.value).toHaveLength(1)
  })

  it('a declined flow is not resurrected by a GET issued before the cancel', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, async () => {
        await held
        return HttpResponse.json({
          data: [flowDto({ stage: 'keys_exchanged', emoji: sevenEmoji() })],
        })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/cancel`,
        () => new HttpResponse(null, { status: 204 }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    const pending = verification.refresh(ACCOUNT)
    await verification.cancel(ACCOUNT, '$flow')
    expect(verification.flows.value).toEqual([])
    release?.()
    await pending
    expect(verification.flows.value).toEqual([])
  })

  it('a dismissed done flow is not resurrected by the terminal grace window', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [flowDto({ stage: 'done' })] }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'done', payload())
    verification.dismissTerminal(flowKey(verification.flows.value[0]))
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value).toEqual([])
    verification.noteFrame(ACCOUNT, 'done', payload())
    expect(verification.flows.value).toEqual([])
  })

  it('a tombstone is retired once the server stops listing the flow', async () => {
    let rows: FlowDto[] = [flowDto({ stage: 'done' })]
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: rows }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'done', payload())
    verification.dismissTerminal(flowKey(verification.flows.value[0]))
    await verification.refresh(ACCOUNT)
    rows = []
    await verification.refresh(ACCOUNT)
    verification.noteFrame(ACCOUNT, 'requested', payload())
    expect(verification.flows.value).toHaveLength(1)
  })

  it('ensureLoaded fetches only the accounts it has not settled', async () => {
    let getsA = 0
    let getsB = 0
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () => {
        getsA += 1
        return HttpResponse.json({ data: [] })
      }),
      http.get(`${BASE}/v1/accounts/${ACCOUNT_B}/verify`, () => {
        getsB += 1
        return HttpResponse.json({ data: [] })
      }),
    )
    const verification = store()
    await verification.ensureLoaded([ACCOUNT])
    await verification.ensureLoaded([ACCOUNT, ACCOUNT_B])
    expect(getsA).toBe(1)
    expect(getsB).toBe(1)
  })

  it('resetSession clears a device load that its own generation bump orphaned', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/devices`, async () => {
        await held
        return HttpResponse.json({ data: { devices: [device(SIBLING)] } })
      }),
    )
    const verification = store()
    const pending = verification.loadDevices(ACCOUNT)
    expect(verification.devicesLoading.value[ACCOUNT]).toBe(true)
    verification.resetSession()
    release?.()
    await pending
    expect(verification.devicesLoading.value[ACCOUNT]).toBeUndefined()
  })

  it('the incomplete-emoji warning clears when the full set arrives', () => {
    const verification = store()
    verification.noteFrame(
      ACCOUNT,
      'sas',
      payload({ emoji: sevenEmoji().slice(0, 1) }),
    )
    expect(verification.flows.value[0].stage).toBe('waiting')
    expect(verification.flows.value[0].error).toBe(INCOMPLETE_EMOJI_MESSAGE)
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].error).toBeNull()
  })

  it('a replayed requested frame does not rewind a flow already at compare', async () => {
    server.use(
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/confirm`,
        () => new HttpResponse(null, { status: 204 }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.noteFrame(ACCOUNT, 'requested', payload())
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].emoji).toHaveLength(7)
    expect(await verification.confirm(ACCOUNT, '$flow')).toEqual({ ok: true })
  })

  it('a late sas or cancelled frame does not rewind a done flow', () => {
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.noteFrame(ACCOUNT, 'done', payload())
    expect(verification.flows.value[0].stage).toBe('done')
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(verification.flows.value[0].stage).toBe('done')
    verification.noteFrame(ACCOUNT, 'cancelled', payload({ reason: 'timeout' }))
    expect(verification.flows.value[0].stage).toBe('done')
  })

  it('a late done frame does not un-cancel an ended flow', () => {
    const verification = store()
    verification.noteFrame(ACCOUNT, 'requested', payload())
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.open(flowKey(verification.flows.value[0]))
    verification.noteFrame(ACCOUNT, 'cancelled', payload({ reason: 'timeout' }))
    expect(verification.flows.value[0].stage).toBe('ended')
    verification.noteFrame(ACCOUNT, 'done', payload())
    expect(verification.flows.value[0].stage).toBe('ended')
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(verification.flows.value[0].stage).toBe('ended')
  })

  it('a stale GET at ready does not rewind a flow already at compare', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [flowDto({ stage: 'ready' })] }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].emoji).toHaveLength(7)
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].emoji).toHaveLength(7)
    expect(verification.flows.value[0].error).toBeNull()
  })

  it('a stale GET at ready does not rewind a flow already confirming', async () => {
    server.use(
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/confirm`,
        () => new HttpResponse(null, { status: 204 }),
      ),
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [flowDto({ stage: 'ready' })] }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(await verification.confirm(ACCOUNT, '$flow')).toEqual({ ok: true })
    expect(verification.flows.value[0].stage).toBe('confirming')
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value[0].stage).toBe('confirming')
    expect(verification.flows.value[0].error).toBeNull()
  })

  it('a stale GET at keys_exchanged does not rewind a completed flow', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({
          data: [
            flowDto({
              stage: 'keys_exchanged',
              emoji: sevenEmoji(),
            }),
          ],
        }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.noteFrame(ACCOUNT, 'done', payload())
    expect(verification.flows.value[0].stage).toBe('done')
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value[0].stage).toBe('done')
    expect(verification.flows.value[0].error).toBeNull()
  })

  it('a stale GET at cancelled does not un-verify a completed flow', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({
          data: [flowDto({ stage: 'cancelled', cancel_reason: 'timeout' })],
        }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.noteFrame(ACCOUNT, 'done', payload())
    expect(verification.flows.value[0].stage).toBe('done')
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value[0].stage).toBe('done')
  })

  it('a stale GET at ready does not rewind a flow already ended', async () => {
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({ data: [flowDto({ stage: 'ready' })] }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    verification.open(flowKey(verification.flows.value[0]))
    verification.noteFrame(ACCOUNT, 'cancelled', payload({ reason: 'timeout' }))
    expect(verification.flows.value[0].stage).toBe('ended')
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value[0].stage).toBe('ended')
  })

  it('a second confirm after success does not revert to compare', async () => {
    let confirms = 0
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/confirm`, () => {
        confirms += 1
        if (confirms === 1) {
          return new HttpResponse(null, { status: 204 })
        }
        return HttpResponse.json(
          {
            error: {
              code: 'conflict',
              message:
                'The flow has not reached the SAS stage, or is already terminal',
            },
          },
          { status: 409 },
        )
      }),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    expect(await verification.confirm(ACCOUNT, '$flow')).toEqual({ ok: true })
    expect(verification.flows.value[0].stage).toBe('confirming')
    expect(await verification.confirm(ACCOUNT, '$flow')).toEqual({ ok: true })
    expect(verification.flows.value[0].stage).toBe('confirming')
    expect(verification.flows.value[0].error).toBeNull()
    expect(confirms).toBe(1)
  })

  it('a mutation error clears when the flow advances', async () => {
    server.use(
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/verify/:flowId/confirm`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'conflict',
              message:
                'The flow has not reached the SAS stage, or is already terminal',
            },
          },
          { status: 409 },
        ),
      ),
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () =>
        HttpResponse.json({
          data: [
            flowDto({
              stage: 'confirmed',
              emoji: sevenEmoji(),
            }),
          ],
        }),
      ),
    )
    const verification = store()
    verification.noteFrame(ACCOUNT, 'sas', payload({ emoji: sevenEmoji() }))
    const result = await verification.confirm(ACCOUNT, '$flow')
    expect(result.ok).toBe(false)
    expect(verification.flows.value[0].stage).toBe('compare')
    expect(verification.flows.value[0].error).toContain(
      'has not reached the SAS stage',
    )
    await verification.refresh(ACCOUNT)
    expect(verification.flows.value[0].stage).toBe('confirming')
    expect(verification.flows.value[0].error).toBeNull()
  })

  it('ensureLoaded is a no-op after a successful refresh of the same set', async () => {
    let gets = 0
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCOUNT}/verify`, () => {
        gets += 1
        return HttpResponse.json({ data: [] })
      }),
    )
    const verification = store()
    await verification.ensureLoaded([ACCOUNT])
    await verification.ensureLoaded([ACCOUNT])
    expect(gets).toBe(1)
  })
})
