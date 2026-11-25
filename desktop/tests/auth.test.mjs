import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  bindMockAuthButton,
  setAuthScreenVisibility,
  syncAuthView,
} from '../src/auth-flow.ts'

class MockButton {
  disabled = false
  textContent = 'Sign In'
  listeners = new Map()

  addEventListener(type, listener) {
    this.listeners.set(type, listener)
  }

  click() {
    return this.listeners.get('click')({ preventDefault() {} })
  }
}

test('sign in click persists mock auth and reveals first-run onboarding', async () => {
  const button = new MockButton()
  const authOverlay = { hidden: false }
  const appShell = { hidden: true }
  const onboarding = {
    open: false,
    showModal() {
      this.open = true
    },
    close() {
      this.open = false
    },
  }
  const calls = []
  let delayMs = 0
  let releaseDelay
  const delayGate = new Promise((resolve) => {
    releaseDelay = resolve
  })

  bindMockAuthButton(button, {
    getEmail: () => 'person@example.test',
    invoke: async (command, args) => {
      calls.push([command, args])
      return command === 'set_auth_status' ? { desktop_preferences: { auth_status: true } } : undefined
    },
    delay: async (milliseconds) => {
      delayMs = milliseconds
      await delayGate
    },
    onAuthenticated: () => syncAuthView(
      { authOverlay, appShell, onboarding },
      true,
      false,
    ),
    onError: (error) => {
      throw error
    },
  })

  const authentication = button.click()
  assert.equal(button.textContent, 'Authenticating...')
  assert.equal(button.disabled, true)
  releaseDelay()
  await authentication

  assert.equal(delayMs, 1500)
  assert.deepEqual(calls, [
    ['mock_login', { email: 'person@example.test' }],
    ['set_auth_status', { status: true }],
  ])
  assert.equal(authOverlay.hidden, true)
  assert.equal(appShell.hidden, false)
  assert.equal(onboarding.open, true)
  assert.equal(button.disabled, false)
  assert.equal(button.textContent, 'Sign In')
})

test('saved sessions skip auth and completed onboarding stays on the dashboard', () => {
  const authOverlay = { hidden: false }
  const appShell = { hidden: true }
  const onboarding = {
    open: false,
    showModal() {
      this.open = true
    },
    close() {
      this.open = false
    },
  }
  const view = { authOverlay, appShell, onboarding }

  syncAuthView(view, true, true)
  assert.equal(authOverlay.hidden, true)
  assert.equal(appShell.hidden, false)
  assert.equal(onboarding.open, false)

  onboarding.open = true
  syncAuthView(view, false, false)
  assert.equal(authOverlay.hidden, false)
  assert.equal(appShell.hidden, true)
  assert.equal(onboarding.open, false)
})

test('login, sign-up, and password-reset screens switch exclusively', () => {
  const screens = {
    login: { hidden: true },
    signup: { hidden: true },
    forgot: { hidden: true },
  }

  setAuthScreenVisibility(screens, 'login')
  assert.deepEqual(
    Object.fromEntries(Object.entries(screens).map(([name, screen]) => [name, !screen.hidden])),
    { login: true, signup: false, forgot: false },
  )

  setAuthScreenVisibility(screens, 'signup')
  assert.deepEqual(
    Object.fromEntries(Object.entries(screens).map(([name, screen]) => [name, !screen.hidden])),
    { login: false, signup: true, forgot: false },
  )

  setAuthScreenVisibility(screens, 'forgot')
  assert.deepEqual(
    Object.fromEntries(Object.entries(screens).map(([name, screen]) => [name, !screen.hidden])),
    { login: false, signup: false, forgot: true },
  )
})
