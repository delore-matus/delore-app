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

type ResourceMode = 'only_when_idle' | 'always_on_low_priority' | 'scheduled_hours'
type ResourcePreset = 'low' | 'balanced' | 'high'

interface ResourceSchedule {
  weekdays: number[]
  start: string
  end: string
}

interface ResourceSettings {
  mode: ResourceMode
  idle_threshold_minutes: number
  schedule: ResourceSchedule | null
  preset: ResourcePreset
  do_not_run_on_battery: boolean
  not_recommended_confirmed: boolean
}

type PolicyDecision = { state: 'run' } | { state: 'paused'; reason: string }

interface PresetMapping {
  num_thread: number
  keep_alive: string
  ollama_num_parallel: number
  max_loaded_models: number
  process_priority: 'normal' | 'below_normal'
  priority_supported: boolean
}

interface ResourceMetrics {
  runtime_cpu_percent: number | null
  runtime_ram_bytes: number | null
  gpu_utilization_percent: number | null
  gpu_vram_used_bytes: number | null
  gpu_device_wide: boolean
}

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
  user_paused: boolean
  payout_address: string | null
  coordinator_ws: string
  configured_model: string
  effective_model: string
  model_override_active: boolean
  configured_model_download_bytes: number | null
  effective_model_download_bytes: number | null
  model_size_error: string | null
  runtime_percent: number
  model_percent: number
  expected_total_download_bytes: number | null
  error_message: string | null
  resource_settings: ResourceSettings
  policy: PolicyDecision
  policy_diagnostic: string | null
  resource_metrics: ResourceMetrics
  preset_mapping: PresetMapping
  requires_restart: boolean
}

const root = document.querySelector<HTMLDivElement>('#app')
if (!root) throw new Error('App root element is missing')

let snapshot: StatusSnapshot | null = null
let addressError = ''
let addressSuccess = ''
let modelError = ''
let modelSuccess = ''
let resourceError = ''
let resourceSuccess = ''
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
      <form class="model-form" id="model-form" novalidate>
        <label for="ollama-model">Configured Ollama model</label>
        <div class="address-form">
          <input id="ollama-model" name="ollama-model" type="text" autocomplete="off"
            spellcheck="false" placeholder="llama3.2:3b" />
          <button class="button button-secondary" id="save-model" type="submit">Save model</button>
        </div>
        <p class="muted" id="configured-model-size">Checking model download size…</p>
        <p class="warning-text" id="model-override" hidden></p>
        <p class="muted" id="effective-model-size" hidden></p>
        <p class="field-error" id="model-error" role="alert" hidden></p>
        <p class="field-success" id="model-success" role="status" hidden></p>
        <p class="muted" id="model-restart-note">Changes apply the next time the provider starts.</p>
      </form>
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

    <section class="panel resource-panel" aria-labelledby="resource-title">
      <div class="panel-heading">
        <div>
          <p class="eyebrow">RESOURCE POLICY</p>
          <h2 id="resource-title">Settings</h2>
        </div>
        <span class="state-chip" id="policy-state">Paused</span>
      </div>
      <p class="policy-reason" id="policy-reason" aria-live="polite">Waiting for policy status…</p>
      <p class="muted policy-diagnostic" id="policy-diagnostic" hidden></p>

      <div class="resource-grid">
        <form id="resource-form" novalidate>
          <label for="resource-mode">Run mode</label>
          <select id="resource-mode" name="mode">
            <option value="only_when_idle">Only when idle</option>
            <option value="always_on_low_priority">Always on (low priority)</option>
            <option value="scheduled_hours">Scheduled hours</option>
          </select>

          <div class="resource-field" id="idle-threshold-field">
            <label for="idle-threshold">Idle threshold (minutes)</label>
            <input id="idle-threshold" name="idle-threshold" type="number" min="1" max="60" step="1" />
            <p class="field-error" id="idle-threshold-error" role="alert" hidden></p>
          </div>

          <div class="resource-field" id="schedule-fields" hidden>
            <p class="field-label">Schedule in local time</p>
            <div class="schedule-times">
              <label for="schedule-start">Start
                <input id="schedule-start" name="schedule-start" type="time" />
              </label>
              <label for="schedule-end">End
                <input id="schedule-end" name="schedule-end" type="time" />
              </label>
            </div>
            <p class="muted" id="schedule-timezone"></p>
            <p class="field-error" id="schedule-error" role="alert" hidden></p>
          </div>

          <label for="resource-preset">Preset</label>
          <select id="resource-preset" name="preset">
            <option value="low">Low</option>
            <option value="balanced">Balanced</option>
            <option value="high">High</option>
          </select>
          <p class="muted" id="preset-mapping"></p>

          <label class="checkbox-label" for="do-not-run-on-battery">
            <input id="do-not-run-on-battery" name="do-not-run-on-battery" type="checkbox" />
            Do not run on battery
          </label>
          <p class="muted">Enabled by default. Unknown power on a device with a battery blocks jobs.</p>

          <p class="field-error" id="resource-error" role="alert" hidden></p>
          <p class="field-success" id="resource-success" role="status" hidden></p>
          <button class="button button-secondary resource-save" id="save-resource-settings" type="submit">
            Save resource settings
          </button>
        </form>

        <div class="resource-monitor" aria-labelledby="monitor-title">
          <p class="eyebrow">MANAGED RUNTIME</p>
          <h3 id="monitor-title">Resource monitor</h3>
          <dl class="metric-list">
            <div><dt>Runtime CPU</dt><dd id="runtime-cpu">Unavailable</dd></div>
            <div><dt>Runtime RAM</dt><dd id="runtime-ram">Unavailable</dd></div>
            <div><dt>GPU utilization · device-wide</dt><dd id="gpu-utilization">Unavailable</dd></div>
            <div><dt>VRAM · device-wide</dt><dd id="gpu-vram">Unavailable</dd></div>
          </dl>
        </div>
      </div>

      <p class="notice restart-notice" id="restart-notice" role="status" hidden>
        A runtime restart is required to apply the selected preset and priority.
        <button class="button button-primary" id="restart-provider" type="button">Restart provider</button>
      </p>
      <p class="muted suitability-detail" id="suitability-detail"></p>
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
        <button class="button button-secondary" id="pause-provider" type="button" disabled>Pause</button>
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
  const modelInput = $<HTMLInputElement>('ollama-model')
  if (document.activeElement !== modelInput) modelInput.value = status.configured_model
  renderResourceSettings(status)

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
  const suitabilityDetail = $<HTMLElement>('suitability-detail')
  if (status.runtime_suitability.status === 'not_recommended') {
    suitabilityDetail.textContent = status.resource_settings.not_recommended_confirmed
      ? 'This hardware is not recommended. Your one-time confirmation is saved; inference will use CPU.'
      : 'This hardware is not recommended. The app will ask for one-time confirmation before preparing the runtime.'
    suitabilityDetail.classList.add('warning-text')
  } else {
    suitabilityDetail.textContent = 'Runtime suitability: suitable.'
    suitabilityDetail.classList.remove('warning-text')
  }
  $<HTMLButtonElement>('start-provider').disabled = status.provider_running || working
  $<HTMLButtonElement>('stop-provider').disabled = !status.provider_running || working
  const pauseButton = $<HTMLButtonElement>('pause-provider')
  pauseButton.textContent = status.user_paused ? 'Resume' : 'Pause'
  pauseButton.disabled = !status.provider_running || working
  $<HTMLButtonElement>('save-address').disabled = working
  modelInput.disabled = status.provider_running || working
  $<HTMLButtonElement>('save-model').disabled = status.provider_running || working
  $<HTMLButtonElement>('restart-provider').disabled =
    !status.provider_running || working || status.current_job.state === 'running'
  $<HTMLButtonElement>('save-resource-settings').disabled = working
  updateAddressFeedback()
  updateModelFeedback(status)
}

function renderResourceSettings(status: StatusSnapshot): void {
  const settings = status.resource_settings
  const mode = $<HTMLSelectElement>('resource-mode')
  const threshold = $<HTMLInputElement>('idle-threshold')
  const preset = $<HTMLSelectElement>('resource-preset')
  const battery = $<HTMLInputElement>('do-not-run-on-battery')
  const schedule = settings.schedule ?? {
    weekdays: [1, 2, 3, 4, 5, 6, 7],
    start: '09:00',
    end: '17:00',
  }
  if (document.activeElement !== mode) mode.value = settings.mode
  if (document.activeElement !== threshold) threshold.value = String(settings.idle_threshold_minutes)
  if (document.activeElement !== preset) preset.value = settings.preset
  if (document.activeElement !== battery) battery.checked = settings.do_not_run_on_battery
  const start = $<HTMLInputElement>('schedule-start')
  const end = $<HTMLInputElement>('schedule-end')
  if (document.activeElement !== start) start.value = schedule.start
  if (document.activeElement !== end) end.value = schedule.end

  $<HTMLDivElement>('idle-threshold-field').hidden = settings.mode !== 'only_when_idle'
  $<HTMLDivElement>('schedule-fields').hidden = settings.mode !== 'scheduled_hours'
  $<HTMLElement>('schedule-timezone').textContent =
    `Timezone: ${Intl.DateTimeFormat().resolvedOptions().timeZone || 'local time'} · ` +
    `days: ${formatWeekdays(schedule.weekdays)}`

  const priority = status.preset_mapping.process_priority === 'below_normal'
    ? 'Below normal'
    : 'Normal'
  const priorityDescription = status.preset_mapping.priority_supported
    ? `${priority} priority`
    : `${priority} priority (not supported on this OS)`
  $<HTMLElement>('preset-mapping').textContent =
    `${status.preset_mapping.num_thread} threads · keep-alive ${status.preset_mapping.keep_alive} · ` +
    `${priorityDescription} · concurrency ${status.preset_mapping.ollama_num_parallel} · ` +
    `max loaded models ${status.preset_mapping.max_loaded_models}`

  const policyChip = $<HTMLElement>('policy-state')
  const policyReason = $<HTMLElement>('policy-reason')
  policyChip.className = 'state-chip'
  if (!status.provider_running) {
    policyChip.textContent = 'Stopped'
    policyReason.textContent = 'Provider is stopped.'
  } else if (status.policy.state === 'paused') {
    policyChip.textContent = 'Paused'
    policyReason.textContent = status.policy.reason
    if (status.policy.reason === 'Paused by user') policyChip.classList.add('state-paused')
  } else if (status.user_paused) {
    policyChip.textContent = 'Pausing'
    policyReason.textContent = 'Pausing after the current job finishes.'
  } else {
    policyChip.textContent = 'Ready'
    policyReason.textContent = status.provider_running ? 'Ready to accept jobs.' : 'Provider is stopped.'
  }
  const diagnostic = $<HTMLParagraphElement>('policy-diagnostic')
  diagnostic.textContent = status.policy_diagnostic ?? ''
  diagnostic.hidden = !status.policy_diagnostic

  const metrics = status.resource_metrics
  $<HTMLElement>('runtime-cpu').textContent = metrics.runtime_cpu_percent === null
    ? 'Unavailable'
    : `${metrics.runtime_cpu_percent.toFixed(1)}%`
  $<HTMLElement>('runtime-ram').textContent = metrics.runtime_ram_bytes === null
    ? 'Unavailable'
    : formatBytes(metrics.runtime_ram_bytes)
  const nvidiaMetricsAvailable =
    status.gpu.support === 'nvidia-supported' && metrics.gpu_device_wide
  $<HTMLElement>('gpu-utilization').textContent =
    nvidiaMetricsAvailable && metrics.gpu_utilization_percent !== null
      ? `${metrics.gpu_utilization_percent}%`
      : 'Unavailable'
  $<HTMLElement>('gpu-vram').textContent =
    nvidiaMetricsAvailable && metrics.gpu_vram_used_bytes !== null
      ? formatBytes(metrics.gpu_vram_used_bytes)
      : 'Unavailable'

  $<HTMLParagraphElement>('resource-error').textContent = resourceError
  $<HTMLParagraphElement>('resource-error').hidden = !resourceError
  $<HTMLParagraphElement>('resource-success').textContent = resourceSuccess
  $<HTMLParagraphElement>('resource-success').hidden = !resourceSuccess
  $<HTMLParagraphElement>('idle-threshold-error').hidden =
    !resourceError.startsWith('Idle threshold:')
  $<HTMLParagraphElement>('idle-threshold-error').textContent =
    resourceError.startsWith('Idle threshold:') ? resourceError : ''
  $<HTMLParagraphElement>('schedule-error').hidden =
    !resourceError.startsWith('Schedule:')
  $<HTMLParagraphElement>('schedule-error').textContent =
    resourceError.startsWith('Schedule:') ? resourceError : ''
  $<HTMLParagraphElement>('resource-error').hidden =
    !resourceError || resourceError.startsWith('Idle threshold:') || resourceError.startsWith('Schedule:')

  const restartNotice = $<HTMLParagraphElement>('restart-notice')
  restartNotice.hidden = !status.requires_restart
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

function formatWeekdays(days: number[]): string {
  const names = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']
  return days.length === 7 ? 'every day' : days.map((day) => names[day - 1] ?? '?').join(', ')
}

function updateAddressFeedback(): void {
  const error = $<HTMLParagraphElement>('address-error')
  const saved = $<HTMLParagraphElement>('address-success')
  error.textContent = addressError
  error.hidden = !addressError
  saved.textContent = addressSuccess
  saved.hidden = !addressSuccess
}

function updateModelFeedback(status: StatusSnapshot): void {
  const configuredSize = $<HTMLParagraphElement>('configured-model-size')
  configuredSize.textContent = status.configured_model_download_bytes === null
    ? 'Configured model download size unavailable.'
    : `Estimated download for ${status.configured_model}: ${formatBytes(status.configured_model_download_bytes)}`

  const override = $<HTMLParagraphElement>('model-override')
  override.hidden = !status.model_override_active
  override.textContent = status.model_override_active
    ? `OLLAMA_MODEL is set and overrides the saved config. Effective model: ${status.effective_model}.`
    : ''

  const effectiveSize = $<HTMLParagraphElement>('effective-model-size')
  effectiveSize.hidden = !status.model_override_active
  effectiveSize.textContent = status.model_override_active
    ? status.effective_model_download_bytes === null
      ? `Effective model download size unavailable for ${status.effective_model}.`
      : `Estimated download for effective model ${status.effective_model}: ${formatBytes(status.effective_model_download_bytes)}`
    : ''

  const error = $<HTMLParagraphElement>('model-error')
  error.textContent = modelError || status.model_size_error || ''
  error.hidden = !error.textContent
  const success = $<HTMLParagraphElement>('model-success')
  success.textContent = modelSuccess
  success.hidden = !modelSuccess
  $<HTMLParagraphElement>('model-restart-note').textContent = status.provider_running
    ? 'Stop the provider before changing the model.'
    : 'Model changes apply the next time the provider starts.'
}

async function startProvider(): Promise<void> {
  if (!snapshot || working) return
  if (
    snapshot.runtime_suitability.status === 'not_recommended' &&
    !snapshot.resource_settings.not_recommended_confirmed
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

$<HTMLSelectElement>('resource-mode').addEventListener('change', () => {
  const mode = $<HTMLSelectElement>('resource-mode').value
  $<HTMLDivElement>('idle-threshold-field').hidden = mode !== 'only_when_idle'
  $<HTMLDivElement>('schedule-fields').hidden = mode !== 'scheduled_hours'
})

$<HTMLFormElement>('resource-form').addEventListener('submit', async (event) => {
  event.preventDefault()
  if (!snapshot || working) return
  resourceError = ''
  resourceSuccess = ''
  const thresholdInput = $<HTMLInputElement>('idle-threshold')
  const idleThreshold = thresholdInput.valueAsNumber
  if (
    (!Number.isInteger(idleThreshold) || idleThreshold < 1 || idleThreshold > 60)
  ) {
    resourceError = 'Idle threshold: enter a whole number from 1 to 60.'
    renderStatus()
    return
  }
  const mode = $<HTMLSelectElement>('resource-mode').value as ResourceMode
  const start = $<HTMLInputElement>('schedule-start').value
  const end = $<HTMLInputElement>('schedule-end').value
  if (mode === 'scheduled_hours' && (!start || !end || start === end)) {
    resourceError = 'Schedule: choose valid, different start and end times.'
    renderStatus()
    return
  }
  const currentSchedule = snapshot.resource_settings.schedule
  const settings: ResourceSettings = {
    ...snapshot.resource_settings,
    mode,
    idle_threshold_minutes: idleThreshold,
    schedule: mode === 'scheduled_hours'
      ? {
          weekdays: currentSchedule?.weekdays ?? [1, 2, 3, 4, 5, 6, 7],
          start,
          end,
        }
      : currentSchedule,
    preset: $<HTMLSelectElement>('resource-preset').value as ResourcePreset,
    do_not_run_on_battery: $<HTMLInputElement>('do-not-run-on-battery').checked,
  }
  working = true
  renderStatus()
  try {
    const next = await invoke<StatusSnapshot>('set_resource_settings', { settings })
    updateStatus(next)
    resourceSuccess = 'Resource settings saved.'
  } catch (error) {
    resourceError = errorMessage(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

$<HTMLButtonElement>('pause-provider').addEventListener('click', async () => {
  if (!snapshot || working) return
  working = true
  renderStatus()
  try {
    await invoke(snapshot.user_paused ? 'resume_provider' : 'pause_provider')
  } catch (error) {
    setCommandError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

$<HTMLButtonElement>('restart-provider').addEventListener('click', async () => {
  if (!snapshot || working || snapshot.current_job.state === 'running') return
  working = true
  renderStatus()
  try {
    await invoke('restart_provider')
  } catch (error) {
    setCommandError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

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

$<HTMLFormElement>('model-form').addEventListener('submit', async (event) => {
  event.preventDefault()
  if (!snapshot || snapshot.provider_running || working) return
  const model = $<HTMLInputElement>('ollama-model').value.trim()
  modelError = ''
  modelSuccess = ''
  working = true
  renderStatus()
  try {
    const next = await invoke<StatusSnapshot>('set_ollama_model', { model })
    updateStatus(next)
    modelSuccess = 'Model saved.'
  } catch (error) {
    modelError = errorMessage(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
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
