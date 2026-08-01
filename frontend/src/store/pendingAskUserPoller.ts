const PENDING_ASK_USER_POLL_INTERVAL_MS = 5000

export interface PendingAskUserPollState {
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error'
  activeSessionId: string | null
  pendingAskUserRefreshInFlight: boolean
}

export interface PendingAskUserPollScheduler {
  schedule: (callback: () => void, delayMs: number) => unknown
  cancel: (timer: unknown) => void
}

interface PendingAskUserPollerOptions {
  readState: () => PendingAskUserPollState
  refresh: () => void
  scheduler: PendingAskUserPollScheduler
}

export class PendingAskUserPoller {
  private readonly readState: () => PendingAskUserPollState
  private readonly refresh: () => void
  private readonly scheduler: PendingAskUserPollScheduler
  private timer: unknown = null
  private stopped = true

  constructor(options: PendingAskUserPollerOptions) {
    this.readState = options.readState
    this.refresh = options.refresh
    this.scheduler = options.scheduler
  }

  start(): void {
    if (!this.stopped) return
    this.stopped = false
    this.scheduleNext()
  }

  stop(): void {
    if (this.stopped) return
    this.stopped = true
    if (this.timer !== null) {
      this.scheduler.cancel(this.timer)
      this.timer = null
    }
  }

  private scheduleNext(): void {
    this.timer = this.scheduler.schedule(() => {
      this.timer = null
      if (this.stopped) return

      const state = this.readState()
      if (
        state.connectionStatus === 'connected' &&
        state.activeSessionId === null &&
        !state.pendingAskUserRefreshInFlight
      ) {
        this.refresh()
      }
      if (!this.stopped) {
        this.scheduleNext()
      }
    }, PENDING_ASK_USER_POLL_INTERVAL_MS)
  }
}
