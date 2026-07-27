import { consumeSseStream } from '../services/sse-stream'
import type { ConversationDelta } from '../services/types'
import { applyDeltaToState } from './delta/applyDelta'
import {
  applyCoalescedDeltas,
  coalesceDeltas,
  isDeferrableDelta,
  type CoalescedDelta,
} from './delta/coalesce'
import {
  SessionStreamController,
  type SessionStreamScheduler,
} from './sessionStreamController'
import type { ActiveSessionStream, AppState } from './types'

const SSE_RECONNECT_BASE_MS = 1000
const SSE_RECONNECT_MAX_MS = 30_000
const STREAM_FLUSH_FALLBACK_MS = 16
type BlockDelta = Exclude<CoalescedDelta, { kind: 'other' }>

const browserScheduler: SessionStreamScheduler = {
  schedule: (callback, delayMs) => window.setTimeout(callback, delayMs),
  cancel: (timer) => window.clearTimeout(timer as number),
  reconnectDelayMs: (attempt) => {
    const capped = Math.min(
      SSE_RECONNECT_MAX_MS,
      SSE_RECONNECT_BASE_MS * 2 ** attempt
    )
    const jitter = Math.random() * 0.3 * capped
    return Math.round(capped + jitter)
  },
}

export function startSessionStream(
  sessionId: string,
  cursor: string,
  get: () => AppState,
  set: (
    partial: Partial<AppState> | ((s: AppState) => Partial<AppState>)
  ) => void
): ActiveSessionStream {
  const pendingDeltas: ConversationDelta[] = []
  let latestCursor: string | null = null
  let rafId: number | null = null
  let timeoutId: number | null = null

  const flushBlockDeltas = (blockDeltas: BlockDelta[]) => {
    if (blockDeltas.length === 0) return
    const deltas = blockDeltas.splice(0)
    set((current) => {
      const { blocks: newBlocks } = applyCoalescedDeltas(current.blocks, deltas)
      return { blocks: newBlocks }
    })
  }

  const clearFlushSchedule = () => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId)
      rafId = null
    }
    if (timeoutId !== null) {
      clearTimeout(timeoutId)
      timeoutId = null
    }
  }

  const flushPending = () => {
    clearFlushSchedule()

    if (pendingDeltas.length === 0) {
      if (latestCursor !== null) {
        set({ cursor: latestCursor })
        latestCursor = null
      }
      return
    }

    const deltas = pendingDeltas.splice(0)
    const cursorUpdate = latestCursor !== null ? { cursor: latestCursor } : null
    latestCursor = null

    const coalesced = coalesceDeltas(deltas)
    const blockDeltas: BlockDelta[] = []

    for (const coalescedDelta of coalesced) {
      if (coalescedDelta.kind === 'other') {
        flushBlockDeltas(blockDeltas)
        applyDeltaToState(get(), coalescedDelta.delta, get, set)
      } else {
        blockDeltas.push(coalescedDelta)
      }
    }

    flushBlockDeltas(blockDeltas)
    if (cursorUpdate) {
      set(cursorUpdate)
    }
  }

  const scheduleFlush = () => {
    if (rafId === null) {
      rafId = requestAnimationFrame(flushPending)
    }
    if (timeoutId === null) {
      timeoutId = window.setTimeout(flushPending, STREAM_FLUSH_FALLBACK_MS)
    }
  }

  const controller = new SessionStreamController({
    sessionId,
    initialCursor: cursor,
    consume: consumeSseStream,
    scheduler: browserScheduler,
    host: {
      isActive: () => get().activeSessionId === sessionId,
      applyEnvelope: (envelope) => {
        if (get().activeSessionId !== sessionId) return
        latestCursor = envelope.cursor.value
        if (isDeferrableDelta(envelope.delta)) {
          pendingDeltas.push(envelope.delta)
          scheduleFlush()
        } else {
          flushPending()
          applyDeltaToState(get(), envelope.delta, get, set)
        }
      },
      rehydrate: () => get().refreshConversationSnapshot(),
      updateStatus: (sessionStreamStatus, sessionStreamError) => {
        if (get().activeSessionId !== sessionId) return
        set({ sessionStreamStatus, sessionStreamError })
      },
    },
  })

  controller.start()
  return {
    stop: () => {
      clearFlushSchedule()
      pendingDeltas.length = 0
      latestCursor = null
      controller.stop()
    },
  }
}
