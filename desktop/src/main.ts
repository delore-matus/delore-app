import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import {
  bindMockAuthButton,
  isDemoAuthEnabled,
  setAuthScreenVisibility,
  syncAuthView,
} from './auth-flow.ts'
import { isValidPerformancePercent } from './performance.ts'
import {
  coordinatorDisplayState,
  preparationDisplayState,
  providerDisplayState,
} from './provider-state.ts'
import './style.css'

type Theme = 'dark' | 'light' | 'system'
type ConnectionState = 'connecting' | 'connected' | 'disconnected'
type PolicyDecision = { state: 'run' } | { state: 'paused'; reason: string }
type UiErrorCategory =
  | 'connection'
  | 'transfer_interrupted'
  | 'not_enough_disk_space'
  | 'unsupported_system'
  | 'invalid_setting'
  | 'settings_unreadable'
  | 'setup_failed'

interface UiError {
  category: UiErrorCategory
  message: string
}

interface DesktopPreferences {
  theme: Theme
  language: 'english'
  onboarding_complete: boolean
  auth_status: boolean
  start_on_login: boolean
  start_minimized_to_tray: boolean
  keep_running_in_tray: boolean
  notifications_enabled: boolean
}

interface SetupState {
  state: string
  phase?: string
  percent?: number
  expected_setup_bytes?: number
  message?: string
}

interface StatusSnapshot {
  provider_running: boolean
  setup_state: SetupState
  hardware_support: { status: 'suitable' | 'not_recommended'; reason?: string }
  gpu: { gpu_name: string; gpu_memory: string; support: string }
  connection: ConnectionState
  fee_config: {
    fee_bps: number
    job_price_units: number
    provider_payout_units: number
    fee_address: string
  } | null
  current_job:
    | { state: 'idle' }
    | { state: 'running'; job_id: string }
    | { state: 'finished'; job_id: string; succeeded: boolean }
  user_paused: boolean
  payout_address: string | null
  coordinator_ws: string
  setup_progress_percent: number
  artifact_progress_percent: number
  expected_setup_bytes: number | null
  error_message: UiError | null
  policy: PolicyDecision
  policy_diagnostic: string | null
  performance_percent: number
  hardware_confirmed: boolean
  desktop_preferences: DesktopPreferences
  earnings: EarningsSnapshot
  earnings_error: UiError | null
}

interface EarningsEntry {
  job_id: string
  timestamp_unix_ms: number
  status: 'completed' | 'failed'
  gross_units: number | null
  provider_payout_units: number | null
  fee_units: number | null
}

interface EarningsSnapshot {
  entries: EarningsEntry[]
  balance_units: number
  job_count: number
  recovered_corruption: boolean
}

const root = document.querySelector<HTMLDivElement>('#app')
if (!root) throw new Error('App root element is missing')

const demoAuthEnabled = isDemoAuthEnabled(import.meta.env.VITE_DEMO_AUTH)
let snapshot: StatusSnapshot | null = null
let currentScreen: 'status' | 'earnings' | 'settings' = 'status'
let working = false
let payoutError = ''
let payoutMessage = ''
let settingsError = ''
let authError = ''
let currentAuthScreen: 'login' | 'signup' | 'forgot' = 'login'

root.innerHTML = `
  <div class="auth-overlay" id="auth-overlay" role="dialog" aria-modal="true" aria-label="DeLore sign in" ${demoAuthEnabled ? '' : 'hidden'}>
    <div class="auth-checking" id="auth-checking" role="status">Checking saved session…</div>

    <div class="auth-card" id="auth-login" aria-labelledby="auth-login-title" hidden>
      <img class="auth-logo" src="/logo.png" alt="DeLore" />
      <p class="eyebrow">LOCAL DEMO SESSION</p>
      <p class="demo-auth-notice">Demo sign-in: no account is created</p>
      <h1 id="auth-login-title">Welcome back to DeLore</h1>
      <p class="muted">Mock sign-in only. Google, GitHub, email authentication and account security are not connected.</p>
      <div class="auth-providers">
        <button class="auth-provider" id="login-google" type="button">
          <svg aria-hidden="true" viewBox="0 0 48 48"><path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5Z"/><path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.74 7.18l7.7 5.98c4.5-4.16 7.08-10.28 7.08-17.63Z"/><path fill="#FBBC05" d="M10.53 28.59A14.4 14.4 0 0 1 9.75 24c0-1.59.27-3.13.76-4.59l-7.98-6.19A23.9 23.9 0 0 0 0 24c0 3.87.93 7.54 2.56 10.78l7.97-6.19Z"/><path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.9-5.82l-7.7-5.98c-2.13 1.43-4.86 2.3-8.2 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48Z"/></svg>
          Continue with Google
        </button>
        <button class="auth-provider" id="login-github" type="button">
          <svg aria-hidden="true" viewBox="0 0 24 24"><path fill="currentColor" d="M12 .9a11.1 11.1 0 0 0-3.51 21.63c.56.1.76-.24.76-.54v-2.08c-3.1.67-3.76-1.32-3.76-1.32-.5-1.28-1.24-1.62-1.24-1.62-1.01-.69.08-.68.08-.68 1.12.08 1.71 1.15 1.71 1.15 1 .1.76 2.4 3.7 1.7.1-.73.39-1.22.7-1.5-2.48-.28-5.09-1.24-5.09-5.52 0-1.22.44-2.22 1.15-3-.12-.28-.5-1.42.11-2.96 0 0 .94-.3 3.05 1.15a10.6 10.6 0 0 1 5.55 0c2.11-1.45 3.04-1.15 3.04-1.15.61 1.54.23 2.68.12 2.96.72.78 1.15 1.78 1.15 3.01 0 4.29-2.61 5.23-5.1 5.51.4.35.75 1.03.75 2.08v3.08c0 .3.2.65.77.54A11.1 11.1 0 0 0 12 .9Z"/></svg>
          Continue with GitHub
        </button>
      </div>
      <div class="auth-divider"><span>or use email</span></div>
      <label for="login-email">Email</label>
      <input id="login-email" type="email" autocomplete="email" required />
      <label for="login-password">Password</label>
      <input id="login-password" type="password" autocomplete="current-password" required />
      <p class="field-error" id="login-error" role="alert" hidden></p>
      <button class="button button-primary auth-submit" id="sign-in" type="button">Sign In</button>
      <div class="auth-links">
        <a href="#" id="forgot-password-link">Forgot password?</a>
        <a href="#" id="sign-up-link">Don't have an account? Sign up</a>
      </div>
    </div>

    <div class="auth-card" id="auth-signup" aria-labelledby="auth-signup-title" hidden>
      <img class="auth-logo" src="/logo.png" alt="DeLore" />
      <p class="eyebrow">LOCAL DEMO SESSION</p>
      <p class="demo-auth-notice">Demo sign-in: no account is created</p>
      <h1 id="auth-signup-title">Create an account</h1>
      <p class="muted">This form demonstrates the interface only; no account is created and passwords are not stored.</p>
      <div class="auth-providers">
        <button class="auth-provider" id="signup-google" type="button">
          <svg aria-hidden="true" viewBox="0 0 48 48"><path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5Z"/><path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.74 7.18l7.7 5.98c4.5-4.16 7.08-10.28 7.08-17.63Z"/><path fill="#FBBC05" d="M10.53 28.59A14.4 14.4 0 0 1 9.75 24c0-1.59.27-3.13.76-4.59l-7.98-6.19A23.9 23.9 0 0 0 0 24c0 3.87.93 7.54 2.56 10.78l7.97-6.19Z"/><path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.9-5.82l-7.7-5.98c-2.13 1.43-4.86 2.3-8.2 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48Z"/></svg>
          Continue with Google
        </button>
        <button class="auth-provider" id="signup-github" type="button">
          <svg aria-hidden="true" viewBox="0 0 24 24"><path fill="currentColor" d="M12 .9a11.1 11.1 0 0 0-3.51 21.63c.56.1.76-.24.76-.54v-2.08c-3.1.67-3.76-1.32-3.76-1.32-.5-1.28-1.24-1.62-1.24-1.62-1.01-.69.08-.68.08-.68 1.12.08 1.71 1.15 1.71 1.15 1 .1.76 2.4 3.7 1.7.1-.73.39-1.22.7-1.5-2.48-.28-5.09-1.24-5.09-5.52 0-1.22.44-2.22 1.15-3-.12-.28-.5-1.42.11-2.96 0 0 .94-.3 3.05 1.15a10.6 10.6 0 0 1 5.55 0c2.11-1.45 3.04-1.15 3.04-1.15.61 1.54.23 2.68.12 2.96.72.78 1.15 1.78 1.15 3.01 0 4.29-2.61 5.23-5.1 5.51.4.35.75 1.03.75 2.08v3.08c0 .3.2.65.77.54A11.1 11.1 0 0 0 12 .9Z"/></svg>
          Continue with GitHub
        </button>
      </div>
      <div class="auth-divider"><span>or use email</span></div>
      <label for="signup-email">Email</label>
      <input id="signup-email" type="email" autocomplete="email" required />
      <label for="signup-password">Password</label>
      <input id="signup-password" type="password" autocomplete="new-password" required />
      <label for="signup-confirm-password">Confirm Password</label>
      <input id="signup-confirm-password" type="password" autocomplete="new-password" required />
      <p class="field-error" id="signup-error" role="alert" hidden></p>
      <button class="button button-primary auth-submit" id="create-account" type="button">Create Account</button>
      <div class="auth-links">
        <a href="#" id="sign-in-link">Already have an account? Sign in</a>
      </div>
    </div>

    <div class="auth-card" id="auth-forgot" aria-labelledby="auth-forgot-title" hidden>
      <img class="auth-logo" src="/logo.png" alt="DeLore" />
      <p class="eyebrow">LOCAL DEMO SESSION</p>
      <p class="demo-auth-notice">Demo sign-in: no account is created</p>
      <h1 id="auth-forgot-title">Reset password</h1>
      <p class="muted">Password reset is unavailable because this demo has no account service.</p>
      <label for="reset-email">Email</label>
      <input id="reset-email" type="email" autocomplete="email" required />
      <p class="field-success" id="reset-message" role="status" hidden></p>
      <button class="button button-primary auth-submit" id="send-reset-link" type="button">Check reset availability</button>
      <div class="auth-links">
        <a href="#" id="back-to-login-link">Back to login</a>
      </div>
    </div>
  </div>

  <main class="shell" id="app-shell" ${demoAuthEnabled ? 'hidden' : ''}>
    <header class="topbar">
      <a class="wordmark" href="#" aria-label="DeLore home"><img src="/logo.png" alt="DeLore" /></a>
      <span class="testnet-badge">TESTNET — no real payouts</span>
    </header>

    <div class="layout">
      <nav class="navigation" aria-label="Main navigation">
        <button class="nav-button active" data-screen="status">Status</button>
        <button class="nav-button" data-screen="earnings">Earnings</button>
        <button class="nav-button" data-screen="settings">Settings</button>
      </nav>

      <div class="content">
        <section class="page-heading">
          <div>
            <p class="eyebrow">DELORE PROVIDER</p>
            <h1 id="page-title">Status</h1>
          </div>
          <div class="connection" id="connection" aria-live="polite">
            <span class="connection-dot" aria-hidden="true"></span>
            <span id="connection-label">Loading status…</span>
          </div>
        </section>

        <p class="notice error-notice" id="global-error" role="alert" hidden></p>

        <section class="screen" id="screen-status">
          <section class="panel">
            <div class="panel-heading">
              <div>
                <p class="eyebrow">PROVIDER</p>
                <h2>Current state</h2>
              </div>
              <span class="state-chip" id="provider-state">Offline</span>
            </div>
            <p class="muted" id="provider-detail">Start DeLore to accept work.</p>
            <div class="status-grid">
              <div>
                <p class="eyebrow">GPU</p>
                <p class="value" id="gpu-name">Detecting…</p>
                <p class="muted" id="gpu-support"></p>
              </div>
              <div>
                <p class="eyebrow">COORDINATOR</p>
                <p class="value" id="coordinator-state">Disconnected</p>
              </div>
              <div>
                <p class="eyebrow">PLATFORM FEE</p>
                <p class="value" id="fee-percent">Waiting for coordinator</p>
              </div>
              <div>
                <p class="eyebrow">FEE RECIPIENT</p>
                <p class="address-value" id="fee-address">Read-only</p>
              </div>
            </div>
          </section>

          <section class="panel">
            <div class="panel-heading">
              <div>
                <p class="eyebrow">PERFORMANCE</p>
                <h2>GPU effort</h2>
              </div>
              <output class="state-chip" id="performance-value">50%</output>
            </div>
            <label class="slider-label" for="performance-slider">Performance</label>
            <input id="performance-slider" type="range" min="0" max="100" step="1" value="50" />
            <div class="slider-limits"><span>0%</span><span>100%</span></div>
            <p class="muted" id="performance-status">
              Approximate limit on how hard DeLore works your GPU.
            </p>
            <p class="muted" id="performance-detail">
              This is an approximate limit, never a hard GPU cap. Pause always overrides the slider.
            </p>
          </section>

          <section class="panel job-panel">
            <div class="panel-heading">
              <div>
                <p class="eyebrow">WORK</p>
                <h2>Current job</h2>
              </div>
              <span class="state-chip" id="job-state">Idle</span>
            </div>
            <p class="job-detail" id="job-detail">No active job.</p>
          </section>

          <section class="panel">
            <div class="panel-heading">
              <div>
                <p class="eyebrow">WALLET</p>
                <h2 id="wallet-status">Wallet not connected</h2>
              </div>
              <span class="state-chip">Not connected</span>
            </div>
            <p class="muted">This testnet app records local ledger entries; no funds are held in escrow.</p>
            <button class="button button-secondary" id="connect-wallet" type="button">Connect Crypto Wallet (MetaMask)</button>
          </section>

          <section class="panel progress-panel" id="preparation-panel" hidden>
            <p class="eyebrow">PREPARING DELORE</p>
            <h2 id="preparation-label">Preparing provider resources</h2>
            <div class="progress-track" id="preparation-progress" role="progressbar"
              aria-label="Provider preparation" aria-valuemin="0" aria-valuemax="100">
              <span id="preparation-bar"></span>
            </div>
            <p class="muted" id="setup-size"></p>
          </section>

          <div class="actions">
            <p class="muted" id="suitability-note"></p>
            <div class="action-buttons">
              <button class="button button-secondary" id="pause-provider" type="button" disabled>Pause</button>
              <button class="button button-secondary" id="stop-provider" type="button" disabled>Stop</button>
              <button class="button button-primary" id="start-provider" type="button">Start</button>
            </div>
          </div>
        </section>

        <section class="screen" id="screen-earnings" hidden>
          <section class="panel">
            <p class="eyebrow">TESTNET BALANCE</p>
            <h2 class="money-value" id="earnings-balance">0 units</h2>
            <span class="money-note">TESTNET - no real payouts</span>
            <div class="earnings-summary">
              <div><span class="muted">Job count</span><strong id="job-count">0</strong></div>
              <div><span class="muted">Stored history</span><strong>On this device</strong></div>
            </div>
          </section>
          <section class="panel">
            <div class="panel-heading">
              <div>
                <p class="eyebrow">RECENT ACTIVITY</p>
                <h2>Jobs</h2>
              </div>
            </div>
            <div id="earnings-error" class="field-error" role="alert" hidden></div>
            <div class="table-wrap">
              <table>
                <thead>
                  <tr><th>Time</th><th>Status</th><th>Provider payout</th><th>Fee</th></tr>
                </thead>
                <tbody id="earnings-rows"></tbody>
              </table>
            </div>
            <p class="empty-state" id="earnings-empty" hidden>No jobs recorded yet.</p>
          </section>
        </section>

        <section class="screen" id="screen-settings" hidden>
          <section class="panel">
            <p class="eyebrow">PREFERENCES</p>
            <h2>Appearance</h2>
            <label for="theme-select">Theme</label>
            <select id="theme-select">
              <option value="dark">Dark</option>
              <option value="light">Light</option>
              <option value="system">System</option>
            </select>
            <label for="language-select">Language</label>
            <select id="language-select" disabled>
              <option value="english">English</option>
            </select>
            <p class="field-error" id="settings-error" role="alert" hidden></p>
          </section>

          <section class="panel">
            <p class="eyebrow">PAYOUT</p>
            <h2>Payout address</h2>
            <p class="muted">Use a public address only. Never enter a private key or seed phrase.</p>
            <form id="payout-form" novalidate>
              <label for="payout-address">Ethereum address</label>
              <div class="address-form">
                <input id="payout-address" name="payout-address" type="text" autocomplete="off"
                  spellcheck="false" placeholder="0x followed by 40 hex characters" />
                <button class="button button-secondary" id="save-address" type="submit">Save address</button>
              </div>
              <p class="field-error" id="address-error" role="alert" hidden></p>
              <p class="field-success" id="address-success" role="status" hidden></p>
            </form>
          </section>

          <section class="panel">
            <p class="eyebrow">STARTUP</p>
            <h2>App behavior</h2>
            <label class="checkbox-label"><input id="start-on-login" type="checkbox" /> Start on login</label>
            <label class="checkbox-label"><input id="start-minimized" type="checkbox" /> Start minimized to tray</label>
            <label class="checkbox-label"><input id="keep-in-tray" type="checkbox" /> Keep running in tray when the window is closed</label>
            <label class="checkbox-label"><input id="notifications-enabled" type="checkbox" /> Notifications</label>
          </section>

          <section class="panel">
            <p class="eyebrow">LOCAL DATA</p>
            <h2>On this device</h2>
            <p class="muted">Settings and job history are stored locally. Open this folder to review or back up your files.</p>
            <div class="about-links">
              <button class="button button-secondary" id="open-data-folder" type="button">Open data folder</button>
              <button class="button button-secondary" id="delete-local-data" type="button">Delete local data</button>
            </div>
          </section>

          ${demoAuthEnabled ? `<section class="panel">
            <p class="eyebrow">ACCOUNT</p>
            <h2>Local demo session</h2>
            <p class="muted">This session is a local interface demo, not an authenticated account.</p>
            <button class="button button-secondary" id="log-out" type="button">Log Out</button>
            <p class="field-error" id="logout-error" role="alert" hidden></p>
          </section>` : ''}

          <section class="panel">
            <p class="eyebrow">ABOUT</p>
            <h2>About DeLore</h2>
            <p class="muted">Version 0.1.0</p>
            <details class="advanced-details">
              <summary>Advanced</summary>
              <label class="settings-field">
                <span>Where DeLore connects</span>
                <input id="coordinator-address" type="text" aria-label="Connection address" readonly>
              </label>
              <p class="muted">This address is managed by the app.</p>
            </details>
            <div class="about-links">
              <a href="#" class="placeholder-link">Terms</a>
              <a href="#" class="placeholder-link">Privacy</a>
              <button class="button button-secondary" id="open-logs" type="button">Open logs</button>
            </div>
          </section>
        </section>
      </div>
    </div>

    <dialog class="confirm-dialog" id="welcome-dialog" aria-labelledby="welcome-title">
      <div>
        <p class="eyebrow">WELCOME</p>
        <h2 id="welcome-title">Welcome to DeLore</h2>
        <p>This app is for testnet use. It does not make real payouts.</p>
        <p>Active work can use your computer's processing, memory, network, and electricity. You can start, pause, or stop work at any time.</p>
        <p>Your settings and job history stay on this device. Open Settings to review or back up the local files.</p>
        <p class="field-error" id="welcome-error" role="alert" hidden></p>
        <div class="dialog-actions">
          <button class="button button-primary" id="complete-onboarding" type="button">Continue</button>
        </div>
      </div>
    </dialog>

    <dialog class="confirm-dialog" id="hardware-confirmation" aria-labelledby="confirm-title">
      <form method="dialog">
        <p class="eyebrow">HARDWARE NOTICE</p>
        <h2 id="confirm-title">Continue on this hardware?</h2>
        <p id="confirm-reason"></p>
        <p class="muted">DeLore will use the CPU for jobs. You can change your mind later.</p>
        <div class="dialog-actions">
          <button class="button button-secondary" value="cancel">Cancel</button>
          <button class="button button-primary" id="confirm-hardware" value="confirm">Continue</button>
        </div>
      </form>
    </dialog>
  </main>
`

const $ = <T extends HTMLElement>(id: string) => {
  const element = document.getElementById(id)
  if (!element) throw new Error(`Missing UI element: ${id}`)
  return element as T
}

function renderAuthView(): void {
  if (!snapshot) return
  const preferences = snapshot.desktop_preferences
  $('auth-checking').hidden = true
  syncAuthView(
    {
      authOverlay: $('auth-overlay'),
      appShell: $('app-shell'),
      onboarding: $<HTMLDialogElement>('welcome-dialog'),
    },
    preferences.auth_status,
    preferences.onboarding_complete,
    demoAuthEnabled,
  )

  if (!demoAuthEnabled) {
    setAuthScreenVisibility(
      {
        login: $('auth-login'),
        signup: $('auth-signup'),
        forgot: $('auth-forgot'),
      },
      null,
    )
    return
  }

  setAuthScreenVisibility(
    {
      login: $('auth-login'),
      signup: $('auth-signup'),
      forgot: $('auth-forgot'),
    },
    preferences.auth_status ? null : currentAuthScreen,
  )
  renderAuthMessages()
}

function renderAuthMessages(): void {
  for (const screen of ['login', 'signup'] as const) {
    const error = $<HTMLParagraphElement>(`${screen}-error`)
    error.textContent = currentAuthScreen === screen ? authError : ''
    error.hidden = currentAuthScreen !== screen || !authError
  }
}

function showAuthScreen(screen: 'login' | 'signup' | 'forgot'): void {
  if (!demoAuthEnabled) return
  currentAuthScreen = screen
  authError = ''
  if ($<HTMLElement>('auth-checking').hidden && snapshot && !snapshot.desktop_preferences.auth_status) {
    renderAuthView()
  } else {
    setAuthScreenVisibility(
      {
        login: $('auth-login'),
        signup: $('auth-signup'),
        forgot: $('auth-forgot'),
      },
      screen,
    )
  }
  if (screen === 'login') $<HTMLInputElement>('login-email').focus()
  if (screen === 'signup') $<HTMLInputElement>('signup-email').focus()
  if (screen === 'forgot') $<HTMLInputElement>('reset-email').focus()
}

function validateAuthFields(
  email: HTMLInputElement,
  password: HTMLInputElement,
  confirmation?: HTMLInputElement,
): boolean {
  authError = ''
  if (!email.checkValidity()) {
    email.reportValidity()
    return false
  }
  if (!password.value) {
    authError = 'Enter your password.'
  } else if (confirmation && password.value !== confirmation.value) {
    authError = 'The passwords do not match.'
  }
  renderAuthMessages()
  return !authError
}

function bindAuthButton(
  id: string,
  getEmail: () => string,
  validate?: () => boolean,
): void {
  bindMockAuthButton<StatusSnapshot>($<HTMLButtonElement>(id), {
    getEmail,
    invoke,
    validate,
    onAuthenticated: (next) => {
      authError = ''
      updateStatus(next)
    },
    onError: (error) => {
      authError = errorMessage(error)
      renderAuthMessages()
    },
  })
}

function updateStatus(next: StatusSnapshot): void {
  snapshot = next
  const coordinatorAddress = document.getElementById('coordinator-address') as HTMLInputElement | null
  if (coordinatorAddress) coordinatorAddress.value = next.coordinator_ws
  renderStatus()
}

function renderStatus(): void {
  if (!snapshot) return
  const status = snapshot
  const prefs = status.desktop_preferences
  applyTheme(prefs.theme)
  const coordinatorState = coordinatorDisplayState(status.connection)
  const connected = status.connection === 'connected'
  $('connection').classList.toggle('is-connected', connected)
  $('connection-label').textContent = coordinatorState.label
  $('coordinator-state').textContent = coordinatorState.status

  $('gpu-name').textContent = status.gpu.gpu_name
  $('gpu-support').textContent = supportDescription(status.gpu.support)
  $('gpu-support').classList.toggle(
    'warning-text',
    !['nvidia-supported', 'apple-silicon-best-effort'].includes(status.gpu.support),
  )
  $('fee-percent').textContent = status.fee_config
    ? `${(status.fee_config.fee_bps / 100).toFixed(status.fee_config.fee_bps % 100 ? 2 : 0)}%`
    : 'Waiting for coordinator'
  $('fee-address').textContent = status.fee_config?.fee_address ?? 'Read-only'

  const job = status.current_job
  const policyReason = status.policy.state === 'paused' ? status.policy.reason : ''
  const providerState = $('provider-state')
  providerState.className = 'state-chip'
  const displayState = providerDisplayState({
    running: status.provider_running,
    connection: status.connection,
    preparing: status.provider_running && status.setup_state.state === 'starting',
    confirmationRequired:
      status.hardware_support.status === 'not_recommended' && !status.hardware_confirmed,
    jobId: job.state === 'running' ? job.job_id : null,
    policyReason,
    userPaused: status.user_paused,
  })
  providerState.textContent = displayState.label
  $('provider-detail').textContent = displayState.detail
  renderJob(job)
  renderPreparation(status)
  renderEarnings(status.earnings)

  $('performance-value').textContent = `${status.performance_percent}%`
  const slider = $<HTMLInputElement>('performance-slider')
  if (document.activeElement !== slider) slider.value = String(status.performance_percent)
  $('performance-status').textContent =
    status.performance_percent === 0
      ? 'DeLore is not taking any work.'
      : 'Approximate limit on how hard DeLore works your GPU.'
  $('suitability-note').textContent =
    status.hardware_support.status === 'not_recommended'
      ? status.hardware_confirmed
        ? `Hardware note: ${status.hardware_support.reason}. CPU processing is used.`
        : `Confirmation is required before work can start: ${status.hardware_support.reason}.`
      : 'Hardware suitability: supported path detected.'

  const globalError = $('global-error')
  globalError.textContent = status.error_message?.message ?? ''
  globalError.hidden = !status.error_message
  $('pause-provider').textContent = status.user_paused ? 'Resume' : 'Pause'
  $<HTMLButtonElement>('start-provider').disabled = status.provider_running || working
  $<HTMLButtonElement>('stop-provider').disabled = !status.provider_running || working
  $<HTMLButtonElement>('pause-provider').disabled = !status.provider_running || working
  $<HTMLButtonElement>('save-address').disabled = working

  const preferences = status.desktop_preferences
  setChecked('start-on-login', preferences.start_on_login)
  setChecked('start-minimized', preferences.start_minimized_to_tray)
  setChecked('keep-in-tray', preferences.keep_running_in_tray)
  setChecked('notifications-enabled', preferences.notifications_enabled)
  const theme = $<HTMLSelectElement>('theme-select')
  if (document.activeElement !== theme) theme.value = preferences.theme
  updatePayoutFeedback()
  renderSettingsError()
  renderAuthView()
}

function renderJob(job: StatusSnapshot['current_job']): void {
  const state = $('job-state')
  const detail = $('job-detail')
  state.className = 'state-chip'
  if (job.state === 'idle') {
    state.textContent = 'Idle'
    detail.textContent = 'No active job.'
  } else if (job.state === 'running') {
    state.textContent = 'Running'
    detail.textContent = `Job ${job.job_id} is running.`
  } else {
    state.textContent = job.succeeded ? 'Complete' : 'Failed'
    if (!job.succeeded) state.classList.add('state-error')
    detail.textContent = `Job ${job.job_id} ${job.succeeded ? 'completed.' : 'failed.'}`
  }
}

function renderPreparation(status: StatusSnapshot): void {
  const panel = $<HTMLElement>('preparation-panel')
  const active = status.provider_running &&
    !['ready', 'running'].includes(status.setup_state.state) &&
    status.setup_state.state !== 'error'
  panel.hidden = !active
  if (!active) return
  const percent = Math.round((status.setup_progress_percent + status.artifact_progress_percent) / 2)
  const preparation = preparationDisplayState(status.setup_state.state, percent)
  panel.classList.toggle('is-indeterminate', preparation.indeterminate)
  $('preparation-label').textContent = preparation.label
  const progress = $('preparation-progress')
  const bar = $<HTMLSpanElement>('preparation-bar')
  if (preparation.indeterminate) {
    bar.style.width = ''
    progress.removeAttribute('aria-valuenow')
    progress.setAttribute('aria-valuetext', preparation.detail)
  } else {
    bar.style.width = `${Math.max(0, Math.min(100, percent))}%`
    progress.setAttribute('aria-valuenow', String(Math.max(0, Math.min(100, percent))))
    progress.setAttribute('aria-valuetext', preparation.label)
  }
  $('setup-size').textContent = preparation.detail || (status.expected_setup_bytes
    ? `Estimated total setup size: ${formatBytes(status.expected_setup_bytes)}`
    : 'Calculating setup size…')
}

function renderEarnings(earnings: EarningsSnapshot): void {
  $('earnings-balance').textContent = `${earnings.balance_units} units`
  $('job-count').textContent = String(earnings.job_count)
  const rows = $('earnings-rows')
  rows.replaceChildren()
  for (const entry of earnings.entries.slice().reverse()) {
    const row = document.createElement('tr')
    row.append(
      cell(new Date(Number(entry.timestamp_unix_ms)).toLocaleString()),
      cell(entry.status === 'completed' ? 'Completed' : 'Failed'),
      moneyCell(entry.provider_payout_units),
      moneyCell(entry.fee_units),
    )
    rows.append(row)
  }
  $('earnings-empty').hidden = earnings.entries.length !== 0
  $('earnings-error').textContent = snapshot?.earnings_error?.message ?? ''
  $('earnings-error').hidden = !snapshot?.earnings_error
}

function cell(value: string): HTMLTableCellElement {
  const td = document.createElement('td')
  td.textContent = value
  return td
}

function moneyCell(value: number | null): HTMLTableCellElement {
  const td = document.createElement('td')
  if (value === null) {
    td.textContent = '—'
    return td
  }
  const amount = document.createElement('span')
  amount.textContent = `${value} units`
  const note = document.createElement('small')
  note.className = 'money-note'
  note.textContent = 'TESTNET - no real payouts'
  td.append(amount, note)
  return td
}

function supportDescription(support: string): string {
  switch (support) {
    case 'integrated-intel-unsupported':
    case 'integrated-amd-best-effort':
      return 'Integrated GPU, not supported: jobs run on CPU'
    case 'none':
      return 'No supported GPU detected: jobs run on CPU'
    case 'nvidia-supported':
      return 'NVIDIA GPU supported'
    case 'apple-silicon-best-effort':
      return 'Apple Silicon detected; GPU use is best-effort'
    default:
      return 'GPU support is best-effort; jobs may run on CPU'
  }
}

function formatBytes(bytes: number): string {
  const gib = bytes / 1024 ** 3
  return gib >= 1 ? `${gib.toFixed(2)} GiB` : `${(bytes / 1024 ** 2).toFixed(0)} MiB`
}

function updatePayoutFeedback(): void {
  $('address-error').textContent = payoutError
  $('address-error').hidden = !payoutError
  $('address-success').textContent = payoutMessage
  $('address-success').hidden = !payoutMessage
  const input = $<HTMLInputElement>('payout-address')
  if (document.activeElement !== input) input.value = snapshot?.payout_address ?? ''
}

function renderSettingsError(): void {
  $('settings-error').textContent = settingsError
  $('settings-error').hidden = !settingsError
}

function setChecked(id: string, value: boolean): void {
  const input = $<HTMLInputElement>(id)
  if (document.activeElement !== input) input.checked = value
}

function applyTheme(theme: Theme): void {
  const resolved = theme === 'system'
    ? (window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark')
    : theme
  document.documentElement.dataset.theme = resolved
}

function showScreen(screen: 'status' | 'earnings' | 'settings'): void {
  currentScreen = screen
  for (const candidate of ['status', 'earnings', 'settings'] as const) {
    $<HTMLElement>(`screen-${candidate}`).hidden = screen !== candidate
    document.querySelector<HTMLButtonElement>(`[data-screen="${candidate}"]`)?.classList.toggle(
      'active',
      screen === candidate,
    )
  }
  $('page-title').textContent = screen[0].toUpperCase() + screen.slice(1)
  if (screen === 'earnings') void refreshEarnings()
}

async function refreshEarnings(): Promise<void> {
  try {
    const earnings = await invoke<EarningsSnapshot>('get_earnings')
    if (snapshot) updateStatus({ ...snapshot, earnings, earnings_error: null })
  } catch (error) {
    if (snapshot) updateStatus({ ...snapshot, earnings_error: uiError(error) })
  }
}

async function startProvider(confirmed = false): Promise<void> {
  if (!snapshot || working) return
  if (
    snapshot.hardware_support.status === 'not_recommended' &&
    !snapshot.hardware_confirmed &&
    !confirmed
  ) {
    $('confirm-reason').textContent = snapshot.hardware_support.reason ?? 'This hardware is not recommended.'
    $<HTMLDialogElement>('hardware-confirmation').showModal()
    return
  }
  working = true
  renderStatus()
  try {
    if (confirmed) await invoke('confirm_hardware_use')
    await invoke('start_provider')
  } catch (error) {
    showError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
}

async function updatePreferences(update: Partial<DesktopPreferences>): Promise<void> {
  if (!snapshot || working) return
  settingsError = ''
  const preferences = { ...snapshot.desktop_preferences, ...update }
  if (update.theme) applyTheme(update.theme)
  working = true
  renderStatus()
  try {
    const next = await invoke<StatusSnapshot>('set_desktop_preferences', { preferences })
    updateStatus(next)
  } catch (error) {
    settingsError = errorMessage(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
}

const uiErrorCategories = new Set<UiErrorCategory>([
  'connection',
  'transfer_interrupted',
  'not_enough_disk_space',
  'unsupported_system',
  'invalid_setting',
  'settings_unreadable',
  'setup_failed',
])

function uiError(error: unknown): UiError {
  const detail = error instanceof Error ? error.message : String(error)
  const separator = detail.indexOf('|')
  if (separator > 0) {
    const category = detail.slice(0, separator) as UiErrorCategory
    if (uiErrorCategories.has(category)) {
      return { category, message: detail.slice(separator + 1) }
    }
  }
  return {
    category: 'setup_failed',
    message: 'DeLore encountered a problem. Check the logs for technical details.',
  }
}

function errorMessage(error: unknown): string {
  return uiError(error).message
}

function showError(error: unknown): void {
  if (!snapshot) return
  updateStatus({ ...snapshot, error_message: uiError(error) })
}

document.querySelectorAll<HTMLButtonElement>('[data-screen]').forEach((button) => {
  button.addEventListener('click', () => showScreen(button.dataset.screen as typeof currentScreen))
})

if (demoAuthEnabled) {
  $<HTMLAnchorElement>('forgot-password-link').addEventListener('click', (event) => {
    event.preventDefault()
    showAuthScreen('forgot')
  })
  $<HTMLAnchorElement>('sign-up-link').addEventListener('click', (event) => {
    event.preventDefault()
    showAuthScreen('signup')
  })
  $<HTMLAnchorElement>('sign-in-link').addEventListener('click', (event) => {
    event.preventDefault()
    showAuthScreen('login')
  })
  $<HTMLAnchorElement>('back-to-login-link').addEventListener('click', (event) => {
    event.preventDefault()
    showAuthScreen('login')
  })

  bindAuthButton(
    'sign-in',
    () => $<HTMLInputElement>('login-email').value.trim(),
    () => validateAuthFields(
      $<HTMLInputElement>('login-email'),
      $<HTMLInputElement>('login-password'),
    ),
  )
  bindAuthButton(
    'create-account',
    () => $<HTMLInputElement>('signup-email').value.trim(),
    () => validateAuthFields(
      $<HTMLInputElement>('signup-email'),
      $<HTMLInputElement>('signup-password'),
      $<HTMLInputElement>('signup-confirm-password'),
    ),
  )
  bindAuthButton('login-google', () => 'google-user@example.invalid')
  bindAuthButton('login-github', () => 'github-user@example.invalid')
  bindAuthButton('signup-google', () => 'google-user@example.invalid')
  bindAuthButton('signup-github', () => 'github-user@example.invalid')

  $<HTMLButtonElement>('send-reset-link').addEventListener('click', () => {
    const email = $<HTMLInputElement>('reset-email')
    if (!email.checkValidity()) {
      email.reportValidity()
      return
    }
    $('reset-message').textContent = 'No email is sent. Password reset is unavailable in this demo.'
    $('reset-message').hidden = false
  })

  $<HTMLButtonElement>('log-out').addEventListener('click', async () => {
    const button = $<HTMLButtonElement>('log-out')
    const error = $<HTMLParagraphElement>('logout-error')
    button.disabled = true
    error.textContent = ''
    error.hidden = true
    try {
      await invoke('set_auth_status', { status: false })
      window.location.reload()
    } catch (reason) {
      error.textContent = errorMessage(reason)
      error.hidden = false
      button.disabled = false
    }
  })
}

$<HTMLButtonElement>('connect-wallet').addEventListener('click', () => {
  window.alert('Wallet connection is not implemented. Earnings are testnet ledger entries; no funds are held in escrow.')
})

$<HTMLInputElement>('performance-slider').addEventListener('change', async (event) => {
  const percent = Number((event.currentTarget as HTMLInputElement).value)
  if (!isValidPerformancePercent(percent)) return
  working = true
  renderStatus()
  try {
    const next = await invoke<StatusSnapshot>('set_performance', { performancePercent: percent })
    updateStatus(next)
  } catch (error) {
    showError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

$<HTMLButtonElement>('start-provider').addEventListener('click', () => void startProvider())
$<HTMLButtonElement>('confirm-hardware').addEventListener('click', (event) => {
  event.preventDefault()
  $<HTMLDialogElement>('hardware-confirmation').close()
  void startProvider(true)
})
$<HTMLButtonElement>('stop-provider').addEventListener('click', async () => {
  working = true
  renderStatus()
  try {
    await invoke('stop_provider')
  } catch (error) {
    showError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})
$<HTMLButtonElement>('pause-provider').addEventListener('click', async () => {
  if (!snapshot) return
  working = true
  renderStatus()
  try {
    await invoke(snapshot.user_paused ? 'resume_provider' : 'pause_provider')
  } catch (error) {
    showError(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

$<HTMLInputElement>('performance-slider').addEventListener('input', (event) => {
  $('performance-value').textContent = `${(event.currentTarget as HTMLInputElement).value}%`
})
$<HTMLSelectElement>('theme-select').addEventListener('change', (event) => {
  void updatePreferences({ theme: (event.currentTarget as HTMLSelectElement).value as Theme })
})
for (const [id, key] of [
  ['start-on-login', 'start_on_login'],
  ['start-minimized', 'start_minimized_to_tray'],
  ['keep-in-tray', 'keep_running_in_tray'],
  ['notifications-enabled', 'notifications_enabled'],
] as const) {
  $<HTMLInputElement>(id).addEventListener('change', (event) => {
    void updatePreferences({ [key]: (event.currentTarget as HTMLInputElement).checked })
  })
}

$<HTMLFormElement>('payout-form').addEventListener('submit', async (event) => {
  event.preventDefault()
  const address = $<HTMLInputElement>('payout-address').value.trim()
  payoutError = ''
  payoutMessage = ''
  working = true
  renderStatus()
  try {
    const next = await invoke<StatusSnapshot>('set_payout_address', { address })
    updateStatus(next)
    payoutMessage = 'Payout address saved.'
  } catch (error) {
    payoutError = errorMessage(error)
  } finally {
    working = false
    if (snapshot) renderStatus()
  }
})

$<HTMLButtonElement>('open-logs').addEventListener('click', async () => {
  try {
    await invoke('open_logs')
  } catch (error) {
    settingsError = errorMessage(error)
    renderSettingsError()
  }
})
$<HTMLButtonElement>('open-data-folder').addEventListener('click', async () => {
  try {
    await invoke('open_data_folder')
  } catch (error) {
    settingsError = errorMessage(error)
    renderSettingsError()
  }
})
$<HTMLButtonElement>('delete-local-data').addEventListener('click', async () => {
  if (!window.confirm('Delete DeLore settings, earnings, and logs from this device? This cannot be undone.')) return
  try {
    await invoke('delete_local_data')
  } catch (error) {
    settingsError = errorMessage(error)
    renderSettingsError()
  }
})
$<HTMLButtonElement>('complete-onboarding').addEventListener('click', async () => {
  if (!snapshot) return
  const button = $<HTMLButtonElement>('complete-onboarding')
  const error = $<HTMLParagraphElement>('welcome-error')
  button.disabled = true
  error.textContent = ''
  error.hidden = true
  try {
    const next = await invoke<StatusSnapshot>('set_desktop_preferences', {
      preferences: { ...snapshot.desktop_preferences, onboarding_complete: true },
    })
    updateStatus(next)
  } catch (reason) {
    error.textContent = errorMessage(reason)
    error.hidden = false
  } finally {
    button.disabled = false
  }
})
$<HTMLDialogElement>('welcome-dialog').addEventListener('cancel', (event) => {
  event.preventDefault()
})
document.querySelectorAll<HTMLAnchorElement>('.placeholder-link').forEach((link) => {
  link.addEventListener('click', (event) => event.preventDefault())
})
window.matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => {
  if (snapshot) applyTheme(snapshot.desktop_preferences.theme)
})

void listen<StatusSnapshot>('provider-status', (event) => updateStatus(event.payload)).catch(showError)
void listen<string>('tray-action', (event) => {
  if (event.payload === 'toggle-provider') {
    if (snapshot?.provider_running) {
      void invoke('stop_provider').catch(showError)
    } else {
      void startProvider()
    }
  } else if (event.payload === 'toggle-pause' && snapshot?.provider_running) {
    void invoke(snapshot.user_paused ? 'resume_provider' : 'pause_provider').catch(showError)
  }
}).catch(showError)
void invoke<StatusSnapshot>('get_status').then(updateStatus).catch((error: unknown) => {
  $('auth-checking').hidden = true
  showAuthScreen('login')
  authError = errorMessage(error)
  renderAuthMessages()
})
