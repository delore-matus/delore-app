import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import './style.css'

type RuntimeState =
  | { state: 'not_installed' }
  | {
      state: 'downloading'
      phase: 'runtime' | 'model'
      percent: number
      bytes: number
      expected_total_download_bytes: number
    }
  | { state: 'error'; message: string }
  | { state: 'verifying' | 'ready' | 'starting' | 'running' | 'stopping' }

type Suitability =
  | { status: 'suitable' }
  | { status: 'not_recommended'; reason: string }

type CurrentJob =
  | { state: 'idle' }
  | { state: 'running'; job_id: string }
  | { state: 'finished'; job_id: string; result_data: string; error: string | null }

interface StatusSnapshot {
  provider_running: boolean
  runtime: RuntimeState
  runtime_suitability: Suitability
  gpu: { gpu_name: string; gpu_memory: string; support: string }
  connection: 'connecting' | 'connected' | 'disconnected'
  fee_config: {
    fee_bps: number
    job_price_units: number
    provider_payout_units: number
    fee_address: string
  } | null
  current_job: CurrentJob
  payout_address: string | null
  coordinator_ws: string
  model: string
  runtime_percent: number
  model_percent: number
  expected_total_download_bytes: number | null
  error_message: string | null
}

const root = document.querySelector<HTMLDivElement>('#app')
if (!root) throw new Error('App root element is missing')

let snapshot: StatusSnapshot | null = null
let addressError = ''
let addressSuccess = ''
let working = false

root.innerHTML = `
  <main class="shell">
    <header class="topbar">
      <a class="wordmark" href="#" aria-label="DeLore home">DeLore</a>
      <span class="testnet-badge">TESTNET — no real payouts</span>
    </header>

    <section class="page-heading" aria-labelledby="page-title">
      <div>
        <p class="eyebrow">PROVIDER</p>
        <h1 id="page-title">Status</h1>
      </div>
      <div class="connection" id="connection" aria-live="polite">
        <span class="connection-dot" aria-hidden="true"></span>
        <span id="connection-label">Loading status…</span>
      </div>
    </section>

    <p class="notice error-notice" id="error-message" role="alert" hidden></p>

    <section class="panel runtime-panel" aria-labelledby="runtime-title">
      <div class="panel-heading">
        <div>
          <p class="eyebrow">INFERENCE RUNTIME</p>
          <h2 id="runtime-title">Runtime and model</h2>
        </div>
        <span class="state-chip" id="runtime-chip">Checking</span>
      </div>
      <div class="download-progress" id="download-progress" hidden>
        <div class="progress-labels">
          <span id="runtime-progress-label">Preparing runtime (0%)</span>
          <span id="model-progress-label">Downloading model (0%)</span>
        </div>
        <div class="progress-track" role="progressbar" aria-label="Runtime and model download">
          <span id="progress-bar"></span>
        </div>
        <p class="muted" id="download-size"></p>
      </div>
      <p class="muted" id="runtime-detail">Reading runtime state…</p>
    </section>

    <div class="details-grid">
      <section class="panel" aria-labelledby="gpu-title">
        <p class="eyebrow">HARDWARE</p>
        <h2 id="gpu-title">GPU</h2>
        <p class="value" id="gpu-name">Detecting…</p>
        <p class="muted" id="gpu-memory"></p>
        <p class="support-note" id="gpu-support"></p>
      </section>

      <section class="panel" aria-labelledby="fee-title">
        <p class="eyebrow">COORDINATOR CONFIGURATION</p>
        <h2 id="fee-title">Platform fee</h2>
        <p class="value" id="fee-percent">Waiting for coordinator</p>
        <p class="muted">Fee recipient</p>
        <p class="address-value" id="fee-address">Read-only; received from coordinator</p>
      </section>
    </div>

    <section class="panel job-panel" aria-labelledby="job-title">
      <div class="panel-heading">
        <div>
          <p class="eyebrow">WORK</p>
          <h2 id="job-title">Current job</h2>
        </div>
        <span class="state-chip" id="job-state">Idle</span>
      </div>
      <p class="job-detail" id="job-detail">No active job.</p>
    </section>

    <section class="panel payout-panel" aria-labelledby="payout-title">
      <div>
        <p class="eyebrow">PROVIDER SETTINGS</p>
        <h2 id="payout-title">Payout address</h2>
        <p class="muted">Use a public address only. Never enter a private key or seed phrase.</p>
      </div>
      <form id="payout-form" novalidate>
        <label for="payout-address">Ethereum address</label>
        <div class="address-form">
          <input id="payout-address" name="payout-address" type="text" inputmode="text"
            autocomplete="off" spellcheck="false" placeholder="0x followed by 40 hex characters" />
          <button class="button button-secondary" id="save-address" type="submit">Save address</button>
        </div>
        <p class="field-error" id="address-error" role="alert" hidden></p>
        <p class="field-success" id="address-success" role="status" hidden></p>
      </form>
    </section>

    <footer class="actions">
      <p class="muted" id="suitability-note"></p>
      <div class="action-buttons">
        <button class="button button-secondary" id="stop-provider" type="button" disabled>Stop</button>
        <button class="button button-primary" id="start-provider" type="button">Start provider</button>
      </div>
    </footer>
  </main>

  <dialog class="confirm-dialog" id="runtime-confirmation" aria-labelledby="confirm-title">
    <form method="dialog">
      <p class="eyebrow">HARDWARE NOTICE</p>
      <h2 id="confirm-title">Run inference on CPU?</h2>
      <p id="confirm-reason"></p>
      <p class="muted">The app will download its private runtime and model into its data directory. No system-wide installation is made.</p>
      <div class="dialog-actions">
        <button class="button button-secondary" value="cancel">Cancel</button>
        <button class="button button-primary" id="confirm-runtime" value="confirm">Continue</button>
      </div>
    </form>
  </dialog>
`

const $ = <T extends HTMLElement>(id: string) => {
  const element = document.getElementById(id)
  if (!element) throw new Error(`Missing UI element: ${id}`)
  return element as T
}

function updateStatus(next: StatusSnapshot): void {
  snapshot = next
  renderStatus()
}

function renderStatus(): void {
  if (!snapshot) return
  const status = snapshot
  const input = $<HTMLInputElement>('payout-address')
  if (document.activeElement !== input) input.value = status.payout_address ?? ''

  const connected = status.connection === 'connected'
  $<HTMLElement>('connection').classList.toggle('is-connected', connected)
  $<HTMLElement>('connection-label').textContent =
    status.connection === 'connected'
      ? 'Coordinator connected'
      : status.connection === 'connecting'
        ? 'Connecting to coordinator'
        : 'Coordinator disconnected'

  $<HTMLElement>('gpu-name').textContent = status.gpu.gpu_name
  $<HTMLElement>('gpu-memory').textContent =
    status.gpu.gpu_memory === 'unknown' ? 'Memory: unknown' : `Memory: ${status.gpu.gpu_memory}`
  $<HTMLElement>('gpu-support').textContent = supportDescription(status.gpu.support)
  $<HTMLElement>('gpu-support').classList.toggle('warning-text', status.gpu.support !== 'nvidia-supported' && status.gpu.support !== 'apple-silicon-best-effort')

  $<HTMLElement>('fee-percent').textContent = status.fee_config
    ? formatPercent(status.fee_config.fee_bps)
    : 'Waiting for coordinator'
  $<HTMLElement>('fee-address').textContent =
    status.fee_config?.fee_address ?? 'Read-only; received from coordinator'

  renderJob(status.current_job)
  renderRuntime(status)

  const error = status.error_message ?? (status.runtime.state === 'error' ? status.runtime.message : '')
  const errorElement = $<HTMLParagraphElement>('error-message')
  errorElement.textContent = error
  errorElement.hidden = !error

  $<HTMLElement>('suitability-note').textContent =
    status.runtime_suitability.status === 'not_recommended'
      ? status.runtime_suitability.reason
      : 'GPU suitability: supported path detected.'
  $<HTMLButtonElement>('start-provider').disabled = status.provider_running || working
  $<HTMLButtonElement>('stop-provider').disabled = !status.provider_running || working
  $<HTMLButtonElement>('save-address').disabled = working
  updateAddressFeedback()
}

function renderRuntime(status: StatusSnapshot): void {
  const runtime = status.runtime
  const chip = $<HTMLElement>('runtime-chip')
  const progress = $<HTMLDivElement>('download-progress')
  const detail = $<HTMLParagraphElement>('runtime-detail')
  chip.className = 'state-chip'
  chip.textContent = runtime.state.replaceAll('_', ' ')
  progress.hidden = true
  detail.textContent = ''

  const preparing =
    runtime.state === 'downloading' ||
    runtime.state === 'verifying' ||
    (status.provider_running && status.expected_total_download_bytes !== null)
  if (preparing) {
    progress.hidden = false
    $<HTMLElement>('runtime-progress-label').textContent =
      `Preparing runtime (${status.runtime_percent}%)`
    $<HTMLElement>('model-progress-label').textContent =
      `Downloading model (${status.model_percent}%)`
    $<HTMLElement>('download-size').textContent =
      status.expected_total_download_bytes
        ? `Expected total download: ${formatBytes(status.expected_total_download_bytes)}`
        : 'Calculating download size…'
    const combined = Math.round((status.runtime_percent + status.model_percent) / 2)
    $<HTMLSpanElement>('progress-bar').style.width = `${Math.max(0, Math.min(100, combined))}%`
    detail.textContent = runtime.state === 'downloading'
      ? runtime.phase === 'runtime' ? 'Preparing the managed Ollama runtime.' : 'Downloading the configured model.'
      : `Runtime ${runtime.state}.`
  } else if (runtime.state === 'not_installed') {
    detail.textContent = 'The private runtime and model will be prepared when the provider starts.'
  } else if (runtime.state === 'error') {
    chip.classList.add('state-error')
    detail.textContent = runtime.message
  } else {
    detail.textContent = 'The managed runtime is installed in app data.'
  }
}

function renderJob(job: CurrentJob): void {
  const state = $<HTMLElement>('job-state')
  const detail = $<HTMLElement>('job-detail')
  state.className = 'state-chip'
  if (job.state === 'idle') {
    state.textContent = 'Idle'
    detail.textContent = 'No active job.'
  } else if (job.state === 'running') {
    state.textContent = 'Running'
    detail.textContent = `Job ${job.job_id} is running.`
  } else {
    state.textContent = job.error ? 'Failed' : 'Complete'
    if (job.error) state.classList.add('state-error')
    detail.textContent = job.error
      ? `Job ${job.job_id}: ${job.error}`
      : `Job ${job.job_id}: ${job.result_data || 'Completed.'}`
  }
}

function supportDescription(support: string): string {
  switch (support) {
    case 'integrated-intel-unsupported':
    case 'integrated-amd-best-effort':
      return 'Integrated GPU, not supported: jobs run on CPU'
    case 'none':
      return 'No supported GPU detected: jobs run on CPU'
    case 'nvidia-supported':
      return 'NVIDIA GPU detected'
    case 'apple-silicon-best-effort':
      return 'Apple Silicon detected; Ollama may use Metal'
    default:
      return 'GPU support is best-effort; jobs may run on CPU'
  }
}

function formatPercent(basisPoints: number): string {
  return `${(basisPoints / 100).toFixed(basisPoints % 100 === 0 ? 0 : 2)}%`
}

function formatBytes(bytes: number): string {
  const gib = bytes / 1024 ** 3
  return gib >= 1 ? `${gib.toFixed(2)} GiB` : `${(bytes / 1024 ** 2).toFixed(0)} MiB`
}

function updateAddressFeedback(): void {
  const error = $<HTMLParagraphElement>('address-error')
  const saved = $<HTMLParagraphElement>('address-success')
  error.textContent = addressError
  error.hidden = !addressError
  saved.textContent = addressSuccess
  saved.hidden = !addressSuccess
}

async function startProvider(): Promise<void> {
  if (!snapshot || working) return
  if (
    (snapshot.runtime.state === 'not_installed' || snapshot.runtime.state === 'error') &&
    snapshot.runtime_suitability.status === 'not_recommended'
  ) {
    $<HTMLElement>('confirm-reason').textContent = snapshot.runtime_suitability.reason
    $<HTMLDialogElement>('runtime-confirmation').showModal()
    return
  }
  await invokeProviderStart(false)
}

async function invokeProviderStart(confirmed: boolean): Promise<void> {
  working = true
  renderStatus()
  try {
    if (confirmed) await invoke('confirm_runtime_download')
    await invoke('start_provider')
  } catch (error) {
    setCommandError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
}

$<HTMLButtonElement>('start-provider').addEventListener('click', () => void startProvider())
$<HTMLButtonElement>('stop-provider').addEventListener('click', async () => {
  working = true
  renderStatus()
  try {
    await invoke('stop_provider')
  } catch (error) {
    setCommandError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

$<HTMLButtonElement>('confirm-runtime').addEventListener('click', (event) => {
  event.preventDefault()
  $<HTMLDialogElement>('runtime-confirmation').close()
  void invokeProviderStart(true)
})

$<HTMLFormElement>('payout-form').addEventListener('submit', async (event) => {
  event.preventDefault()
  const address = $<HTMLInputElement>('payout-address').value.trim()
  addressError = ''
  addressSuccess = ''
  working = true
  renderStatus()
  try {
    const next = await invoke<StatusSnapshot>('set_payout_address', { address })
    updateStatus(next)
    addressSuccess = 'Payout address saved.'
  } catch (error) {
    addressError = errorMessage(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function setCommandError(error: unknown): void {
  if (!snapshot) return
  snapshot = { ...snapshot, error_message: errorMessage(error) }
  renderStatus()
}

void listen<StatusSnapshot>('provider-status', (event) => updateStatus(event.payload)).catch(setCommandError)
void invoke<StatusSnapshot>('get_status').then(updateStatus).catch(setCommandError)
