import { consumeSseStream } from '../services/sse-stream'
import type { ConversationDelta } from '../services/types'
import { applyDeltaToState } from './delta/applyDelta'
import {
  applyCoalescedDeltas,
  coalesceDeltas,
  isDeferrableDelta,
  type CoalescedDelta,
} from './delta/coalesce'
import type { AppState } from './types'

const SSE_RECONNECT_BASE_MS = 1000
const SSE_RECONNECT_MAX_MS = 30_000
const STREAM_FLUSH_FALLBACK_MS = 16
type BlockDelta = Exclude<CoalescedDelta, { kind: 'other' }>

function sseReconnectDelayMs(attempt: number): number {
  const capped = Math.min(
    SSE_RECONNECT_MAX_MS,
    SSE_RECONNECT_BASE_MS * 2 ** attempt
  )
  const jitter = Math.random() * 0.3 * capped
  return Math.round(capped + jitter)
}

export function connectSse(
  sessionId: string,
  cursor: string,
  reconnectAttempt: number,
  get: () => AppState,
  set: (
    partial: Partial<AppState> | ((s: AppState) => Partial<AppState>)
  ) => void
): void {
  const abortController = new AbortController()
  set({ streamAbortController: abortController })

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

  const flushPending = () => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId)
      rafId = null
    }
    if (timeoutId !== null) {
      clearTimeout(timeoutId)
      timeoutId = null
    }

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

    for (const c of coalesced) {
      if (c.kind === 'other') {
        flushBlockDeltas(blockDeltas)
        applyDeltaToState(get(), c.delta, get, set)
      } else {
        blockDeltas.push(c)
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

  consumeSseStream(
    sessionId,
    cursor,
    (envelope) => {
      const current = get()
      if (current.activeSessionId !== sessionId) return
      if (envelope.cursor) {
        latestCursor = envelope.cursor.value
      }
      if (isDeferrableDelta(envelope.delta)) {
        pendingDeltas.push(envelope.delta)
        scheduleFlush()
      } else {
        flushPending()
        applyDeltaToState(current, envelope.delta, get, set)
      }
    },
    abortController.signal
  )
    .then((result) => {
      if (abortController.signal.aborted) return
      if (result === 'ended') {
        const current = get()
        if (current.activeSessionId === sessionId) {
          const latestCursor = current.cursor ?? cursor
          const delayMs = sseReconnectDelayMs(reconnectAttempt)
          setTimeout(() => {
            if (get().activeSessionId === sessionId) {
              connectSse(
                sessionId,
                latestCursor,
                reconnectAttempt + 1,
                get,
                set
              )
            }
          }, delayMs)
        }
      }
    })
    .catch((err) => {
      if (abortController.signal.aborted) return
      const delayMs = sseReconnectDelayMs(reconnectAttempt)
      console.error('SSE stream error, reconnecting in', delayMs, 'ms:', err)
      if (get().activeSessionId === sessionId) {
        const latestCursor = get().cursor ?? cursor
        setTimeout(() => {
          if (get().activeSessionId === sessionId) {
            connectSse(sessionId, latestCursor, reconnectAttempt + 1, get, set)
          }
        }, delayMs)
      }
    })
}
