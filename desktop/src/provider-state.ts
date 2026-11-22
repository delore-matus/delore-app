export interface ProviderDisplayInput {
  running: boolean
  confirmationRequired: boolean
  jobId: string | null
  policyReason: string
  userPaused: boolean
}

export interface ProviderDisplayState {
  label: string
  detail: string
}

export function providerDisplayState(input: ProviderDisplayInput): ProviderDisplayState {
  if (!input.running && input.confirmationRequired) {
    return {
      label: 'Needs confirmation',
      detail: 'Confirm hardware use before starting.',
    }
  }
  if (!input.running) {
    return { label: 'Offline', detail: 'Start DeLore to accept work.' }
  }
  if (input.jobId !== null) {
    return { label: 'Running a job', detail: `Job ${input.jobId} is in progress.` }
  }
  if (input.policyReason) {
    return { label: 'Paused', detail: input.policyReason }
  }
  if (input.userPaused) {
    return {
      label: 'Pausing',
      detail: 'The current job will finish before new work pauses.',
    }
  }
  return { label: 'Online', detail: 'Ready to accept work.' }
}
