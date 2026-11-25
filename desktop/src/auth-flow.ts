type AuthButton = Pick<HTMLButtonElement, 'addEventListener' | 'disabled' | 'textContent'>
type AuthDialog = Pick<HTMLDialogElement, 'open' | 'showModal' | 'close'>

export interface AuthViewElements {
  authOverlay: Pick<HTMLElement, 'hidden'>
  appShell: Pick<HTMLElement, 'hidden'>
  onboarding: AuthDialog
}

export interface AuthScreens {
  login: Pick<HTMLElement, 'hidden'>
  signup: Pick<HTMLElement, 'hidden'>
  forgot: Pick<HTMLElement, 'hidden'>
}

export interface AuthButtonOptions<T> {
  getEmail: () => string
  invoke: <TResult>(command: string, args?: Record<string, unknown>) => Promise<TResult>
  onAuthenticated: (status: T) => void
  onError: (error: unknown) => void
  validate?: () => boolean
  delay?: (milliseconds: number) => Promise<void>
}

export function syncAuthView(
  elements: AuthViewElements,
  authenticated: boolean,
  onboardingComplete: boolean,
): void {
  elements.authOverlay.hidden = authenticated
  elements.appShell.hidden = !authenticated

  if (!authenticated) {
    if (elements.onboarding.open) elements.onboarding.close()
    return
  }

  if (!onboardingComplete && !elements.onboarding.open) {
    elements.onboarding.showModal()
  } else if (onboardingComplete && elements.onboarding.open) {
    elements.onboarding.close()
  }
}

export function setAuthScreenVisibility(
  screens: AuthScreens,
  activeScreen: 'login' | 'signup' | 'forgot' | null,
): void {
  for (const screen of ['login', 'signup', 'forgot'] as const) {
    screens[screen].hidden = activeScreen !== screen
  }
}

export function bindMockAuthButton<T>(
  button: AuthButton,
  options: AuthButtonOptions<T>,
): void {
  button.addEventListener('click', async (event) => {
    event.preventDefault()
    if (button.disabled || (options.validate && !options.validate())) return

    const originalText = button.textContent
    button.disabled = true
    button.textContent = 'Authenticating...'

    try {
      await (options.delay ?? delay)(1500)
      await options.invoke<void>('mock_login', { email: options.getEmail() })
      const status = await options.invoke<T>('set_auth_status', { status: true })
      options.onAuthenticated(status)
    } catch (error) {
      options.onError(error)
    } finally {
      button.disabled = false
      button.textContent = originalText
    }
  })
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds))
}
