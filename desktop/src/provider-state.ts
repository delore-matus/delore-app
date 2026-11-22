export interface ProviderDisplayInput {
  running: boolean
  connection: 'connecting' | 'connected' | 'disconnected'
  preparing: boolean
  confirmationRequired: boolean
  jobId: string | null
  policyReason: string
  userPaused: boolean
}

export interface ProviderDisplayState {
  label: string
  detail: string
}

export interface CoordinatorDisplayState {
  label: string
  status: string
}

export interface PreparationDisplayState {
  label: string
  detail: string
  indeterminate: boolean
}

export function preparationDisplayState(
  state: string,
  percent: number,
): PreparationDisplayState {
  if (state === 'starting') {
    return {
      label: 'Preparing DeLore',
      detail: 'Starting DeLore. This can take up to 3 minutes.',
      indeterminate: true,
    }
  }
  return {
    label: `Preparing DeLore (${percent}%)`,
    detail: '',
    indeterminate: false,
  }
}

export function coordinatorDisplayState(
  connection: 'connecting' | 'connected' | 'disconnected',
): CoordinatorDisplayState {
  switch (connection) {
    case 'connected':
      return { label: 'Coordinator connected', status: 'Connected' }
    case 'connecting':
      return { label: 'Connecting to coordinator', status: 'Connecting' }
    case 'disconnected':
      return { label: 'Coordinator unreachable', status: 'Coordinator unreachable' }
  }
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
  if (input.preparing) {
    return {
      label: 'Preparing',
      detail: 'Preparing DeLore before connecting to the coordinator.',
    }
  }
  if (input.connection === 'connecting') {
    return { label: 'Connecting', detail: 'Waiting for the coordinator.' }
  }
  if (input.connection === 'disconnected') {
    return {
      label: 'Coordinator unreachable',
      detail: 'Waiting to reconnect to the coordinator.',
    }
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
