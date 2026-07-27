import type { ConversationStreamEnvelope } from '../services/types'
import { SessionStreamProtocolError } from '../services/streamErrors'

export { SessionStreamProtocolError } from '../services/streamErrors'

export type SessionStreamStatus =
  | 'disconnected'
  | 'connecting'
  | 'connected'
  | 'reconnecting'
  | 'degraded'

export type SessionStreamConsumer = (
  sessionId: string,
  cursor: string | null,
  onEnvelope: (envelope: ConversationStreamEnvelope) => void,
  signal: AbortSignal,
  onOpen: () => void
) => Promise<'ended' | 'aborted'>

export interface SessionStreamHost {
  isActive: () => boolean
  applyEnvelope: (envelope: ConversationStreamEnvelope) => void
  rehydrate: () => Promise<string | null>
  updateStatus: (status: SessionStreamStatus, error: string | null) => void
}

export interface SessionStreamScheduler {
  schedule: (callback: () => void, delayMs: number) => unknown
  cancel: (timer: unknown) => void
  reconnectDelayMs: (attempt: number) => number
}

interface SessionStreamControllerOptions {
  sessionId: string
  initialCursor: string | null
  consume: SessionStreamConsumer
  host: SessionStreamHost
  scheduler: SessionStreamScheduler
}

export class SessionStreamController {
  private readonly sessionId: string
  private readonly consume: SessionStreamConsumer
  private readonly host: SessionStreamHost
  private readonly scheduler: SessionStreamScheduler
  private cursor: string | null
  private abortController: AbortController | null = null
  private reconnectTimer: unknown = null
  private stopped = true
  private hasConnected = false

  constructor(options: SessionStreamControllerOptions) {
    this.sessionId = options.sessionId
    this.cursor = options.initialCursor
    this.consume = options.consume
    this.host = options.host
    this.scheduler = options.scheduler
  }

  start(): void {
    if (!this.stopped) return
    this.stopped = false
    void this.connect(0)
  }

  stop(): void {
    if (this.stopped) return
    this.stopped = true
    this.abortController?.abort()
    this.abortController = null
    if (this.reconnectTimer !== null) {
      this.scheduler.cancel(this.reconnectTimer)
      this.reconnectTimer = null
    }
  }

  private async connect(attempt: number): Promise<void> {
    if (!this.canContinue()) return

    const abortController = new AbortController()
    this.abortController = abortController
    this.host.updateStatus(
      this.hasConnected ? 'reconnecting' : 'connecting',
      null
    )

    let opened = false
    try {
      const result = await this.consume(
        this.sessionId,
        this.cursor,
        (envelope) => {
          if (!this.canContinue()) return
          if (envelope.cursor) {
            this.cursor = envelope.cursor.value
          }
          this.host.applyEnvelope(envelope)
        },
        abortController.signal,
        () => {
          if (!this.canContinue()) return
          opened = true
          this.hasConnected = true
          this.host.updateStatus('connected', null)
        }
      )
      if (result === 'ended' && this.canContinue()) {
        this.host.updateStatus('reconnecting', null)
        this.scheduleReconnect(opened ? 0 : attempt + 1, false)
      }
    } catch (error) {
      if (abortController.signal.aborted || !this.canContinue()) return
      const requiresRehydrate = error instanceof SessionStreamProtocolError
      this.host.updateStatus(
        requiresRehydrate ? 'degraded' : 'reconnecting',
        error instanceof Error ? error.message : String(error)
      )
      this.scheduleReconnect(
        opened && !requiresRehydrate ? 0 : attempt + 1,
        requiresRehydrate
      )
    } finally {
      if (this.abortController === abortController) {
        this.abortController = null
      }
    }
  }

  private scheduleReconnect(attempt: number, requiresRehydrate: boolean): void {
    if (!this.canContinue()) return
    if (this.reconnectTimer !== null) {
      this.scheduler.cancel(this.reconnectTimer)
    }
    this.reconnectTimer = this.scheduler.schedule(() => {
      this.reconnectTimer = null
      void this.resume(attempt, requiresRehydrate)
    }, this.scheduler.reconnectDelayMs(attempt))
  }

  private async resume(
    attempt: number,
    requiresRehydrate: boolean
  ): Promise<void> {
    if (!this.canContinue()) return
    if (requiresRehydrate) {
      const cursor = await this.host.rehydrate()
      if (!this.canContinue()) return
      if (cursor === null) {
        this.scheduleReconnect(attempt + 1, true)
        return
      }
      this.cursor = cursor
    }
    await this.connect(attempt)
  }

  private canContinue(): boolean {
    return !this.stopped && this.host.isActive()
  }
}
