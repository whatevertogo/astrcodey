import { consumeSseStream } from '../services/sse-stream'
import { applyDeltasToState } from './delta/applyDelta'
import {
  SessionStreamController,
  type SessionStreamScheduler,
} from './sessionStreamController'
import { ConversationDeltaFrameBuffer } from './delta/frameBuffer'
import type { ActiveSessionStream, AppState } from './types'
import { MAX_TIMELINE_BLOCKS } from './conversationHistory'

const SSE_RECONNECT_BASE_MS = 1000
const SSE_RECONNECT_MAX_MS = 30_000
const STREAM_FLUSH_FALLBACK_MS = 16

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
  const frameBuffer = new ConversationDeltaFrameBuffer()
  let rafId: number | null = null
  let timeoutId: number | null = null
  let rebasingTimeline = false

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
    if (frameBuffer.isEmpty()) return

    const frame = frameBuffer.drain()
    applyDeltasToState(frame.deltas, get, set, frame.cursor ?? undefined)
    const state = get()
    if (
      !rebasingTimeline &&
      !state.timelineDetachedFromLatest &&
      state.blocks.length > MAX_TIMELINE_BLOCKS
    ) {
      rebasingTimeline = true
      void state.returnToLatestConversation().finally(() => {
        rebasingTimeline = false
      })
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
        const shouldFlush = frameBuffer.push(
          envelope.delta,
          envelope.cursor.value
        )
        if (shouldFlush) {
          flushPending()
        } else {
          scheduleFlush()
        }
      },
      rehydrate: async () => {
        flushPending()
        if (get().activeSessionId !== sessionId) return null
        return get().refreshConversationSnapshot()
      },
      updateStatus: (sessionStreamStatus, sessionStreamError) => {
        if (get().activeSessionId !== sessionId) return
        set({ sessionStreamStatus, sessionStreamError })
        if (sessionStreamStatus === 'connected') {
          void get().refreshPendingAskUserQuestions()
        }
      },
    },
  })

  controller.start()
  return {
    stop: () => {
      clearFlushSchedule()
      frameBuffer.clear()
      controller.stop()
    },
  }
}
