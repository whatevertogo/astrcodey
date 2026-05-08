import { create } from 'zustand'
import * as api from '../services/api'
import { consumeSseStream } from '../services/sse-stream'
import { getHostBridge } from '../lib/hostBridge'
import type {
  ConversationBlock,
  ConversationControlState,
  ConversationDelta,
  SessionListItem,
  Phase,
} from '../services/types'

interface ConversationState {
  serverPort: number | null
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error'
  connectionError: string | null

  sessions: SessionListItem[]
  activeSessionId: string | null
  activeSessionTitle: string | null
  workingDir: string | null

  blocks: ConversationBlock[]
  control: ConversationControlState | null
  cursor: string | null
  phase: Phase

  streamAbortController: AbortController | null
  modelRefreshKey: number

  initServer: () => Promise<void>
  refreshSessions: () => Promise<void>
  createSession: (workingDir: string) => Promise<void>
  deleteSession: (sessionId: string) => Promise<void>
  deleteProject: (workingDir: string) => Promise<void>
  bumpModelRefreshKey: () => void
  switchSession: (sessionId: string) => Promise<void>
  submitPrompt: (text: string) => Promise<boolean>
  abortCurrentTurn: () => Promise<void>
  compactSession: () => Promise<void>
  applyDelta: (delta: ConversationDelta) => void
}

function phaseFromControl(control: ConversationControlState | null): Phase {
  return control?.phase ?? 'idle'
}

export const useAppStore = create<ConversationState>((set, get) => ({
  serverPort: null,
  connectionStatus: 'disconnected',
  connectionError: null,
  sessions: [],
  activeSessionId: null,
  activeSessionTitle: null,
  workingDir: null,
  blocks: [],
  control: null,
  cursor: null,
  phase: 'idle',
  streamAbortController: null,
  modelRefreshKey: 0,

  initServer: async () => {
    set({ connectionStatus: 'connecting', connectionError: null })

    const bridge = getHostBridge()

    if (bridge.isDesktopHost) {
      try {
        const { invoke } = await import('@tauri-apps/api/core')
        const port = await invoke<number>('start_server')
        api.setServerPort(port)
        set({ serverPort: port })
      } catch (err) {
        set({
          connectionStatus: 'error',
          connectionError: err instanceof Error ? err.message : String(err),
        })
        return
      }
    } else {
      api.initBaseUrl()
    }

    set({ connectionStatus: 'connected' })
    await get().refreshSessions()
  },

  refreshSessions: async () => {
    try {
      const response = await api.listSessions()
      set({ sessions: response.sessions })
    } catch (err) {
      console.error('Failed to refresh sessions:', err)
    }
  },

  createSession: async (workingDir: string) => {
    const response = await api.createSession(workingDir)
    await get().refreshSessions()
    await get().switchSession(response.sessionId)
  },

  deleteSession: async (sessionId: string) => {
    try {
      await api.deleteSession(sessionId)
    } catch (err) {
      console.error('Failed to delete session:', err)
    }
    const state = get()
    if (state.activeSessionId === sessionId) {
      state.streamAbortController?.abort()
      set({
        activeSessionId: null,
        activeSessionTitle: null,
        blocks: [],
        control: null,
        cursor: null,
        phase: 'idle',
        workingDir: null,
      })
    }
    await get().refreshSessions()
  },

  deleteProject: async (workingDir: string) => {
    try {
      await api.deleteProject(workingDir)
    } catch (err) {
      console.error('Failed to delete project:', err)
    }
    const state = get()
    const activeSession = state.sessions.find(
      (s) => s.sessionId === state.activeSessionId
    )
    if (activeSession && activeSession.workingDir === workingDir) {
      state.streamAbortController?.abort()
      set({
        activeSessionId: null,
        activeSessionTitle: null,
        blocks: [],
        control: null,
        cursor: null,
        phase: 'idle',
        workingDir: null,
      })
    }
    await get().refreshSessions()
  },

  bumpModelRefreshKey: () => {
    set((s) => ({ modelRefreshKey: s.modelRefreshKey + 1 }))
  },

  switchSession: async (sessionId: string) => {
    const state = get()

    state.streamAbortController?.abort()

    set({
      activeSessionId: sessionId,
      blocks: [],
      control: null,
      cursor: null,
      phase: 'idle',
    })

    try {
      const snapshot = await api.getConversation(sessionId)
      const sessions = get().sessions
      const sessionItem = sessions.find((s) => s.sessionId === sessionId)

      set({
        blocks: snapshot.blocks,
        control: snapshot.control,
        cursor: snapshot.cursor.value,
        phase: phaseFromControl(snapshot.control),
        activeSessionTitle: snapshot.sessionTitle,
        workingDir: sessionItem?.workingDir ?? null,
      })

      connectSse(sessionId, snapshot.cursor.value, get, set)
    } catch (err) {
      console.error('Failed to switch session:', err)
    }
  },

  submitPrompt: async (text: string) => {
    const { activeSessionId, control } = get()
    if (!activeSessionId || !control?.canSubmitPrompt) return false

    await api.submitPrompt(activeSessionId, text)
    return true
  },

  abortCurrentTurn: async () => {
    const { activeSessionId } = get()
    if (!activeSessionId) return

    await api.abortSession(activeSessionId)
  },

  compactSession: async () => {
    const { activeSessionId, control } = get()
    if (!activeSessionId || !control?.canRequestCompact) return

    const response = await api.compactSession(activeSessionId)
    await get().refreshSessions()
    if (response.newSessionId) {
      await get().switchSession(response.newSessionId)
    } else {
      await get().switchSession(activeSessionId)
    }
  },

  applyDelta: (delta: ConversationDelta) => {
    const state = get()

    switch (delta.kind) {
      case 'appendBlock':
        set({ blocks: [...state.blocks, delta.block] })
        break

      case 'patchBlock': {
        const idx = state.blocks.findIndex(
          (b) => 'id' in b && b.id === delta.blockId
        )
        if (idx === -1) break
        const block = state.blocks[idx]
        if (block.kind === 'assistant' || block.kind === 'toolCall') {
          const next = [...state.blocks]
          next[idx] = { ...block, text: block.text + delta.textDelta }
          set({ blocks: next })
        }
        break
      }

      case 'completeBlock': {
        const idx = state.blocks.findIndex(
          (b) => 'id' in b && b.id === delta.blockId
        )
        if (idx === -1) break
        const block = state.blocks[idx]
        if (block.kind === 'assistant' || block.kind === 'toolCall') {
          const next = [...state.blocks]
          next[idx] = { ...block, status: 'complete' as const }
          set({ blocks: next })
        }
        break
      }

      case 'updateControlState':
        set({
          control: delta.control,
          phase: phaseFromControl(delta.control),
        })
        break

      case 'toolOutput': {
        const idx = state.blocks.findIndex(
          (b) => b.kind === 'toolCall' && b.id === delta.callId
        )
        if (idx === -1) break
        const block = state.blocks[idx]
        if (block.kind !== 'toolCall') break
        const prefix = delta.stream === 'stderr' ? '\n[stderr] ' : '\n'
        const next = [...state.blocks]
        next[idx] = { ...block, text: block.text + prefix + delta.delta }
        set({ blocks: next })
        break
      }

      case 'rehydrateRequired': {
        const sessionId = state.activeSessionId
        if (sessionId) {
          void get().switchSession(sessionId)
        }
        break
      }

      case 'sessionContinued': {
        void get().refreshSessions()
        void get().switchSession(delta.newSessionId)
        break
      }
    }
  },
}))

const SSE_RECONNECT_DELAY_MS = 3000

function connectSse(
  sessionId: string,
  cursor: string,
  get: () => ConversationState,
  set: (
    partial:
      | Partial<ConversationState>
      | ((s: ConversationState) => Partial<ConversationState>)
  ) => void
): void {
  const abortController = new AbortController()
  set({ streamAbortController: abortController })

  consumeSseStream(
    sessionId,
    cursor,
    (envelope) => {
      const current = get()
      if (current.activeSessionId !== sessionId) return
      if (envelope.cursor) {
        set({ cursor: envelope.cursor.value })
      }
      current.applyDelta(envelope.delta)
    },
    abortController.signal
  )
    .then((result) => {
      if (abortController.signal.aborted) return
      if (result === 'ended') {
        const current = get()
        if (current.activeSessionId === sessionId) {
          const latestCursor = current.cursor ?? cursor
          setTimeout(() => {
            if (get().activeSessionId === sessionId) {
              connectSse(sessionId, latestCursor, get, set)
            }
          }, SSE_RECONNECT_DELAY_MS)
        }
      }
    })
    .catch((err) => {
      if (abortController.signal.aborted) return
      console.error(
        'SSE stream error, reconnecting in',
        SSE_RECONNECT_DELAY_MS,
        'ms:',
        err
      )
      if (get().activeSessionId === sessionId) {
        const latestCursor = get().cursor ?? cursor
        setTimeout(() => {
          if (get().activeSessionId === sessionId) {
            connectSse(sessionId, latestCursor, get, set)
          }
        }, SSE_RECONNECT_DELAY_MS)
      }
    })
}
