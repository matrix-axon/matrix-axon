import { useCallback, useEffect, useRef, useState } from 'preact/hooks'
import { CopyableText } from '../../components/CopyableText'
import { ErrorBanner } from '../../components/ErrorBanner'
import { useServices } from '../../services'
import { hasActiveAccount } from '../../stores/accounts'
import {
  isTerminalMatrixOAuthQrFlow,
  type MatrixOAuthQrFlow,
} from '../../stores/matrix-oauth-qr'
import type { QrCameraDevice, QrCameraSession } from '../../qr/browser-qr'

const STAGE_SUMMARIES: Record<MatrixOAuthQrFlow['stage'], string> = {
  starting: 'Preparing QR sign-in…',
  qr_ready: 'QR code ready. Scan it with your trusted Matrix device.',
  check_code_to_display:
    'Compare this check code with the code on your trusted device.',
  check_code_required:
    'Enter the two-digit check code shown by your trusted device.',
  waiting_for_authorization:
    'Approve the Matrix device authorization request to continue.',
  syncing_secrets: 'Authorization complete. Synchronizing encryption secrets…',
  done: 'Account signed in and this Axon device is verified.',
  failed: 'QR sign-in failed.',
  cancelled: 'QR sign-in cancelled.',
}

function safeVerificationUri(value: string | null | undefined): string | null {
  if (value === null || value === undefined) {
    return null
  }
  try {
    const url = new URL(value)
    const loopbackHttp =
      url.protocol === 'http:' &&
      (url.hostname === 'localhost' ||
        url.hostname === '::1' ||
        url.hostname === '[::1]' ||
        /^127(?:\.[0-9]{1,3}){3}$/.test(url.hostname))
    return url.protocol === 'https:' || loopbackHttp ? url.href : null
  } catch {
    return null
  }
}

function QrCanvas({ data }: { data: string }) {
  const { qr } = useServices()
  const canvas = useRef<HTMLCanvasElement>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    const target = canvas.current
    if (target === null) {
      return
    }
    let alive = true
    void Promise.resolve()
      .then(() => qr.decodeBase64(data))
      .then((bytes) => qr.render(target, bytes))
      .then(() => alive && setError(null))
      .catch((cause: unknown) => {
        if (alive) {
          setError(
            cause instanceof Error
              ? cause.message
              : 'The QR code could not be rendered.',
          )
        }
      })
    return () => {
      alive = false
      target.width = 0
      target.height = 0
    }
  }, [data, qr])

  return (
    <div class="qr-display">
      <canvas ref={canvas} aria-label="Matrix sign-in QR code" role="img" />
      {error !== null && <p class="field-hint error">{error}</p>}
    </div>
  )
}

function QrScanner() {
  const { matrixOAuthQr, qr } = useServices()
  const video = useRef<HTMLVideoElement>(null)
  const session = useRef<QrCameraSession | null>(null)
  const cameraGeneration = useRef(0)
  const selectedCameraRef = useRef<string | null>(null)
  const [cameraStarting, setCameraStarting] = useState(false)
  const [cameraActive, setCameraActive] = useState(false)
  const [cameras, setCameras] = useState<QrCameraDevice[]>([])
  const [selectedCamera, setSelectedCamera] = useState<string | null>(null)
  const [cameraError, setCameraError] = useState<string | null>(null)
  const [cameraListError, setCameraListError] = useState<string | null>(null)
  const [imageError, setImageError] = useState<string | null>(null)
  const [decodingImage, setDecodingImage] = useState(false)

  const selectCamera = useCallback((deviceId: string | null) => {
    selectedCameraRef.current = deviceId
    setSelectedCamera(deviceId)
  }, [])

  const stopCamera = useCallback(() => {
    cameraGeneration.current += 1
    session.current?.stop()
    session.current = null
    setCameraStarting(false)
    setCameraActive(false)
  }, [])

  const refreshCameras = useCallback(
    async (owner: number, fromDeviceChange = false) => {
      try {
        const available = await qr.listCameras()
        if (owner !== cameraGeneration.current) {
          return
        }
        setCameraListError(null)
        setCameras(available)
        const activeDeviceId =
          session.current?.deviceId ?? selectedCameraRef.current
        const activeStillAvailable = available.some(
          (camera) => camera.deviceId === activeDeviceId,
        )
        if (
          fromDeviceChange &&
          session.current !== null &&
          activeDeviceId !== null &&
          !activeStillAvailable
        ) {
          stopCamera()
          selectCamera(available[0]?.deviceId ?? null)
          setCameraError('The selected camera is no longer available.')
          return
        }
        selectCamera(
          activeStillAvailable
            ? activeDeviceId
            : (available[0]?.deviceId ?? null),
        )
      } catch (cause) {
        if (owner === cameraGeneration.current) {
          setCameraListError(
            cause instanceof Error
              ? `Camera choices could not be loaded: ${cause.message}`
              : 'Camera choices could not be loaded.',
          )
        }
      }
    },
    [qr, selectCamera, stopCamera],
  )

  useEffect(() => {
    const unwatch = qr.watchCameras(() => {
      void refreshCameras(cameraGeneration.current, true)
    })
    return () => {
      unwatch()
      cameraGeneration.current += 1
      session.current?.stop()
      session.current = null
    }
  }, [qr, refreshCameras])

  const submitBytes = (bytes: Uint8Array) => {
    stopCamera()
    void matrixOAuthQr.submitScan(qr.encodeBase64(bytes))
  }

  const startCamera = async (
    deviceId: string | null = selectedCameraRef.current,
  ) => {
    const target = video.current
    if (target === null) {
      return
    }
    stopCamera()
    const owner = cameraGeneration.current
    setCameraError(null)
    setCameraListError(null)
    setCameraStarting(true)
    try {
      const started = await qr.startCamera(
        target,
        submitBytes,
        setCameraError,
        deviceId ?? undefined,
      )
      if (owner !== cameraGeneration.current) {
        started.stop()
        return
      }
      session.current = started
      selectCamera(started.deviceId ?? deviceId)
      setCameraActive(true)
      await refreshCameras(owner)
    } catch (cause) {
      if (owner === cameraGeneration.current) {
        setCameraError(
          cause instanceof Error
            ? cause.message
            : 'Camera permission was not granted.',
        )
      }
    } finally {
      if (owner === cameraGeneration.current) {
        setCameraStarting(false)
      }
    }
  }

  return (
    <div class="qr-scanner">
      <p>
        Start a camera, or choose an image containing the QR code from your
        trusted device. Camera choices appear after permission is granted.
      </p>
      <video
        ref={video}
        aria-label="Matrix QR camera preview"
        playsInline
        muted
      />
      {cameras.length > 1 && (
        <label class="qr-camera-picker">
          Camera
          <select
            aria-label="Camera"
            value={selectedCamera ?? cameras[0].deviceId}
            disabled={
              cameraStarting || matrixOAuthQr.operation.value !== 'idle'
            }
            onChange={(event) => {
              const deviceId = event.currentTarget.value
              selectCamera(deviceId)
              void startCamera(deviceId)
            }}
          >
            {cameras.map((camera) => (
              <option key={camera.deviceId} value={camera.deviceId}>
                {camera.label}
              </option>
            ))}
          </select>
        </label>
      )}
      <div class="card-actions">
        <button
          type="button"
          disabled={cameraStarting || matrixOAuthQr.operation.value !== 'idle'}
          onClick={() => void startCamera()}
        >
          {cameraStarting ? 'Starting camera…' : 'Start camera'}
        </button>
        {cameraActive && (
          <button type="button" onClick={stopCamera}>
            Stop camera
          </button>
        )}
        <label class="button-like">
          Choose QR image
          <input
            class="visually-hidden"
            type="file"
            accept="image/*"
            disabled={decodingImage || matrixOAuthQr.operation.value !== 'idle'}
            onChange={(event) => {
              const input = event.currentTarget
              const file = input.files?.[0]
              if (file === undefined) {
                return
              }
              stopCamera()
              setImageError(null)
              setDecodingImage(true)
              void qr
                .scanImage(file)
                .then(submitBytes)
                .catch((cause: unknown) =>
                  setImageError(
                    cause instanceof Error
                      ? cause.message
                      : 'The selected image could not be decoded.',
                  ),
                )
                .finally(() => {
                  setDecodingImage(false)
                  input.value = ''
                })
            }}
          />
        </label>
      </div>
      {cameraError !== null && (
        <p class="field-hint error">{cameraError} Choose an image instead.</p>
      )}
      {cameraListError !== null && (
        <p class="field-hint error">{cameraListError}</p>
      )}
      {imageError !== null && <p class="field-hint error">{imageError}</p>}
    </div>
  )
}

function ActiveQrFlow({ flow }: { flow: MatrixOAuthQrFlow }) {
  const { matrixOAuthQr } = useServices()
  const [checkCode, setCheckCode] = useState('')
  const operation = matrixOAuthQr.operation.value
  const busy = operation !== 'idle' && operation !== 'polling'
  const verificationUri = safeVerificationUri(flow.verification_uri)

  return (
    <div class="qr-flow">
      <p class="qr-stage-summary" aria-live="polite" aria-atomic="true">
        {STAGE_SUMMARIES[flow.stage]}
      </p>

      {flow.stage === 'starting' && flow.presentation === 'scan' && (
        <QrScanner />
      )}
      {flow.stage === 'qr_ready' &&
        flow.qr_code_data !== null &&
        flow.qr_code_data !== undefined && (
          <QrCanvas data={flow.qr_code_data} />
        )}
      {flow.stage === 'check_code_required' && (
        <form
          class="inline-form"
          onSubmit={(event) => {
            event.preventDefault()
            if (/^[0-9]{2}$/.test(checkCode)) {
              void matrixOAuthQr.submitCheckCode(checkCode)
            }
          }}
        >
          <label class="segmented-code-label">
            Two-digit check code
            <span class="segmented-code">
              <span class="segmented-code-cells" aria-hidden="true">
                {[0, 1].map((index) => (
                  <span
                    key={index}
                    class={`segmented-code-cell${
                      checkCode[index] === undefined ? '' : ' filled'
                    }${
                      index === Math.min(checkCode.length, 1) ? ' current' : ''
                    }`}
                  >
                    {checkCode[index] ?? ''}
                  </span>
                ))}
              </span>
              <input
                class="segmented-code-input"
                value={checkCode}
                inputMode="numeric"
                autoComplete="one-time-code"
                pattern="[0-9]{2}"
                maxLength={2}
                onInput={(event) =>
                  setCheckCode(
                    event.currentTarget.value
                      .replace(/[^0-9]/g, '')
                      .slice(0, 2),
                  )
                }
              />
            </span>
          </label>
          <button
            type="submit"
            disabled={busy || !/^[0-9]{2}$/.test(checkCode)}
          >
            Confirm code
          </button>
        </form>
      )}
      {flow.stage === 'check_code_to_display' &&
        flow.check_code !== null &&
        flow.check_code !== undefined && (
          <div class="qr-check-code" aria-label="Check code">
            {flow.check_code}
          </div>
        )}
      {flow.stage === 'waiting_for_authorization' && (
        <div class="authorization-step">
          {flow.authorization_user_code !== null &&
            flow.authorization_user_code !== undefined && (
              <CopyableText
                text={flow.authorization_user_code}
                label="authorization user code"
              >
                <code class="authorization-code">
                  {flow.authorization_user_code}
                </code>
              </CopyableText>
            )}
          {verificationUri !== null ? (
            <p>
              <a href={verificationUri}>Open the secure verification page</a>
            </p>
          ) : flow.verification_uri !== null &&
            flow.verification_uri !== undefined ? (
            <p class="field-hint error">
              Axon returned an unsafe verification link. Open your authorization
              service directly instead.
            </p>
          ) : null}
        </div>
      )}

      <ErrorBanner error={matrixOAuthQr.error} />

      {!['done', 'failed', 'cancelled'].includes(flow.stage) && (
        <button
          type="button"
          disabled={operation === 'cancelling'}
          onClick={() => void matrixOAuthQr.cancel()}
        >
          {operation === 'cancelling' ? 'Cancelling…' : 'Cancel QR sign-in'}
        </button>
      )}
      {['failed', 'cancelled'].includes(flow.stage) && (
        <button type="button" onClick={() => matrixOAuthQr.reset()}>
          Start again
        </button>
      )}
      {flow.stage === 'done' && (
        <button type="button" onClick={() => matrixOAuthQr.reset()}>
          Add another account
        </button>
      )}
    </div>
  )
}

function MismatchedQrFlow({
  flow,
  expectedUserId,
}: {
  flow: MatrixOAuthQrFlow
  expectedUserId: string
}) {
  const { matrixOAuthQr } = useServices()
  const cancelling = matrixOAuthQr.operation.value === 'cancelling'

  const clearPreviousFlow = async () => {
    if (!isTerminalMatrixOAuthQrFlow(flow) && !(await matrixOAuthQr.cancel())) {
      return
    }
    matrixOAuthQr.reset()
  }

  return (
    <div class="qr-flow">
      <p class="qr-stage-summary" role="status">
        QR sign-in for {flow.expected_user_id} is still active. Cancel it before
        starting QR reactivation for {expectedUserId}.
      </p>
      <ErrorBanner error={matrixOAuthQr.error} />
      <button
        type="button"
        disabled={cancelling}
        onClick={() => void clearPreviousFlow()}
      >
        {cancelling ? 'Cancelling…' : 'Cancel previous QR sign-in'}
      </button>
    </div>
  )
}

export function MatrixOAuthQrAcquisition({
  expectedUserId,
  onSuccess,
}: {
  expectedUserId?: string
  onSuccess?: () => void
}) {
  const { accounts, matrixOAuthQr } = useServices()
  const [userId, setUserId] = useState('')
  const [presentation, setPresentation] = useState<'display' | 'scan'>(
    'display',
  )
  const completionHandled = useRef(false)
  const navigateAfterCompletion = useRef<boolean | null>(null)
  const flow = matrixOAuthQr.flow.value
  const effectiveUserId = expectedUserId ?? userId
  const flowMatchesExpectedUser =
    flow === null ||
    expectedUserId === undefined ||
    flow.expected_user_id === expectedUserId

  useEffect(() => {
    void matrixOAuthQr.resume()
  }, [matrixOAuthQr])

  useEffect(() => {
    if (flow === null) {
      completionHandled.current = false
      if (matrixOAuthQr.operation.value !== 'starting') {
        navigateAfterCompletion.current = null
      }
      return
    }
    if (!flowMatchesExpectedUser) {
      return
    }
    if (!accounts.loading.value && navigateAfterCompletion.current === null) {
      navigateAfterCompletion.current = !hasActiveAccount(
        accounts.accounts.value,
      )
    }
    if (
      flow.stage === 'done' &&
      matrixOAuthQr.operation.value === 'idle' &&
      !accounts.loading.value &&
      navigateAfterCompletion.current !== null &&
      !completionHandled.current
    ) {
      completionHandled.current = true
      onSuccess?.()
      if (!navigateAfterCompletion.current) {
        return
      }
      history.pushState(null, '', '/')
      window.dispatchEvent(new PopStateEvent('popstate'))
    }
  }, [
    accounts.accounts.value,
    accounts.loading.value,
    flow,
    flowMatchesExpectedUser,
    matrixOAuthQr.operation.value,
    onSuccess,
  ])

  if (flow !== null) {
    if (!flowMatchesExpectedUser && expectedUserId !== undefined) {
      return <MismatchedQrFlow flow={flow} expectedUserId={expectedUserId} />
    }
    return <ActiveQrFlow flow={flow} />
  }

  return (
    <form
      class="stack-form qr-start-form"
      onSubmit={(event) => {
        event.preventDefault()
        navigateAfterCompletion.current = !hasActiveAccount(
          accounts.accounts.value,
        )
        completionHandled.current = false
        void matrixOAuthQr.start(effectiveUserId.trim(), presentation)
      }}
    >
      <label>
        Expected Matrix user ID
        <input
          value={effectiveUserId}
          placeholder="@alice:example.org"
          readOnly={expectedUserId !== undefined}
          onInput={(event) => setUserId(event.currentTarget.value)}
        />
      </label>
      <fieldset>
        <legend>Which device will show the QR code?</legend>
        <label>
          <input
            type="radio"
            name="qr-presentation"
            value="display"
            checked={presentation === 'display'}
            onChange={() => setPresentation('display')}
          />
          Show a QR code on this device
        </label>
        <label>
          <input
            type="radio"
            name="qr-presentation"
            value="scan"
            checked={presentation === 'scan'}
            onChange={() => setPresentation('scan')}
          />
          Scan a QR code with this device
        </label>
      </fieldset>
      <button
        type="submit"
        disabled={
          effectiveUserId.trim() === '' ||
          matrixOAuthQr.operation.value === 'starting'
        }
      >
        {matrixOAuthQr.operation.value === 'starting'
          ? 'Preparing…'
          : 'Start QR sign-in'}
      </button>
      <ErrorBanner error={matrixOAuthQr.error} />
    </form>
  )
}
