import { create } from 'zustand'
import * as api from '../services/api'
import { resolveHostBridge } from '../lib/hostBridge'
import type { ConversationDelta } from '../services/types'
import { applyDeltaToState } from './delta/applyDelta'
import { isRegisteredSlashCommand } from '../lib/keybindings'
import {
  commandNoteBlock,
  isCompactCommand,
  resolvePhase,
  withTimeout,
} from './delta/blockHelpers'
import { connectSse } from './stream'
import { canInjectMidTurn, isExecutionPhase } from './phaseHelpers'
import {
  computeInitialProjectFolderOrder,
  syncProjectFolderOrder,
} from '../components/Sidebar/projectFolderOrder'
import type { AppState } from './types'

function resetSessionView(): Partial<AppState> {
  return {
    activeSessionId: null,
    activeSessionTitle: null,
    blocks: [],
    control: null,
    cursor: null,
    phase: 'idle',
    compactSubmitting: false,
    workingDir: null,
    agentSessions: [],
    pendingMessages: [],
    composerDeliveryMode: 'queued',
    slashCommands: [],
    keybindings: [],
    statusItems: {},
  }
}

export const useAppStore = create<AppState>((set, get) => ({
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
  compactSubmitting: false,
  streamAbortController: null,
  modelRefreshKey: 0,
  agentSessions: [],
  statusItems: {},
  keybindings: [],
  slashCommands: [],
  extensions: [],
  transientHint: null,
  pendingMessages: [],
  composerDeliveryMode: 'queued',
  projectFolderOrder: [],

  initServer: async () => {
    set({ connectionStatus: 'connecting', connectionError: null })

    const bridge = await resolveHostBridge()

    if (bridge.isDesktopHost) {
      try {
        const { invoke } = await import('@tauri-apps/api/core')
        const result = await withTimeout(
          invoke<{ port: number; token?: string }>('start_server'),
          15_000,
          '启动 AstrCode 服务超时，请关闭残留 astrcode-http-server 进程后重试'
        )
        api.setServerPort(result.port, result.token)
        set({ serverPort: result.port })
      } catch (err) {
        set({
          connectionStatus: 'error',
          connectionError: err instanceof Error ? err.message : String(err),
        })
        return
      }
    } else {
      api.initBaseUrl()
      const envToken = (
        import.meta as unknown as { env: Record<string, string> }
      ).env?.VITE_AUTH_TOKEN
      if (envToken) {
        api.setAuthToken(envToken)
      }
    }

    set({ connectionStatus: 'connected' })
    await get().refreshSessions()
    void get().refreshExtensionData()
  },

  refreshSessions: async () => {
    try {
      const response = await api.listSessions()
      set((current) => ({
        sessions: response.sessions,
        projectFolderOrder:
          current.projectFolderOrder.length === 0
            ? computeInitialProjectFolderOrder(response.sessions)
            : syncProjectFolderOrder(
                current.projectFolderOrder,
                response.sessions
              ),
      }))
    } catch (err) {
      console.error('Failed to refresh sessions:', err)
      set({
        transientHint: err instanceof Error ? err.message : '刷新会话列表失败',
      })
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
      set(resetSessionView())
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
      set(resetSessionView())
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
      compactSubmitting: false,
      agentSessions: [],
      transientHint: null,
      pendingMessages: [],
      composerDeliveryMode: 'queued',
      slashCommands: [],
      keybindings: [],
      statusItems: {},
    })

    try {
      const snapshot = await api.getConversation(sessionId)
      const sessions = get().sessions
      const sessionItem = sessions.find((s) => s.sessionId === sessionId)

      set({
        blocks: snapshot.blocks,
        control: snapshot.control,
        cursor: snapshot.cursor.value,
        phase: resolvePhase(snapshot.control, false),
        activeSessionTitle: snapshot.sessionTitle,
        workingDir: sessionItem?.workingDir ?? null,
        agentSessions: snapshot.agentSessions ?? [],
      })

      connectSse(sessionId, snapshot.cursor.value, 0, get, set)
      void get().refreshCommands()
    } catch (err) {
      console.error('Failed to switch session:', err)
      set({
        transientHint:
          err instanceof Error ? err.message : '加载会话失败，请重试',
      })
    }
  },

  refreshConversationSnapshot: async () => {
    const { activeSessionId } = get()
    if (!activeSessionId) return

    try {
      const snapshot = await api.getConversation(activeSessionId)
      set({
        blocks: snapshot.blocks,
        control: snapshot.control,
        cursor: snapshot.cursor.value,
        phase: resolvePhase(snapshot.control, get().compactSubmitting),
        activeSessionTitle: snapshot.sessionTitle,
        agentSessions: snapshot.agentSessions ?? [],
      })
    } catch (err) {
      console.error('Failed to refresh conversation snapshot:', err)
      set({
        transientHint: err instanceof Error ? err.message : '刷新会话快照失败',
      })
    }
  },

  refreshExtensionData: async () => {
    try {
      const extensions = await api.listExtensions()
      set({ extensions })
    } catch (err) {
      console.error('Failed to refresh extensions:', err)
      set({
        transientHint: err instanceof Error ? err.message : '刷新扩展数据失败',
      })
    }
  },

  executeExtensionCommand: async (command: string, argumentsText = '') => {
    const { activeSessionId } = get()
    if (!activeSessionId) return false

    try {
      const response = await api.executeExtensionCommand(
        activeSessionId,
        command,
        argumentsText
      )
      if (
        (response.kind === 'handled' || response.kind === 'display') &&
        get().activeSessionId !== response.sessionId
      ) {
        return true
      }
      // Display 类扩展命令由 SSE ExtensionCommandResult → appendBlock 展示，勿重复写入。
      return true
    } catch (err) {
      console.error('executeExtensionCommand failed:', err)
      set({
        transientHint: err instanceof Error ? err.message : '命令执行失败',
      })
      return false
    }
  },

  refreshCommands: async () => {
    const { activeSessionId } = get()
    if (!activeSessionId) return

    try {
      const response = await api.listCommands(activeSessionId)
      const statusItems: Record<string, string> = {}
      for (const item of response.statusItems) {
        statusItems[item.id] = item.text
      }
      set({
        slashCommands: response.commands,
        keybindings: response.keybindings,
        statusItems,
      })
    } catch (err) {
      console.error('Failed to refresh commands:', err)
      set({
        transientHint: err instanceof Error ? err.message : '刷新命令列表失败',
      })
    }
  },

  submitPrompt: async (
    text: string,
    attachments: import('../services/types').PromptAttachmentWire[] = []
  ) => {
    const state = get()
    const { activeSessionId } = state
    if (!activeSessionId) {
      return false
    }

    const compactCommand = isCompactCommand(text)
    if (compactCommand) {
      set({ compactSubmitting: true, phase: 'compacting' })
    }

    const busy = isExecutionPhase(state.phase, state.compactSubmitting)
    const slashCommand = isRegisteredSlashCommand(text, state.slashCommands)
    const injectable = canInjectMidTurn(state.control, state.compactSubmitting)

    try {
      if (busy && !compactCommand && !slashCommand) {
        if (state.composerDeliveryMode === 'inject') {
          if (!injectable) {
            set({
              transientHint: '当前无法 inject，已改为加入队列',
            })
            set((current) => ({
              pendingMessages: [
                ...current.pendingMessages,
                {
                  id: crypto.randomUUID(),
                  text,
                  delivery: 'queued',
                },
              ],
            }))
            return true
          }
          await api.injectMessage(activeSessionId, text)
          return true
        }

        set((current) => ({
          pendingMessages: [
            ...current.pendingMessages,
            {
              id: crypto.randomUUID(),
              text,
              delivery: 'queued',
            },
          ],
        }))
        return true
      }

      const response = await api.submitPrompt(
        activeSessionId,
        text,
        attachments
      )
      if (response.kind === 'accepted') {
        set((current) => ({
          phase: 'thinking',
          control: {
            phase: 'thinking',
            canSubmitPrompt: false,
            canRequestCompact: current.control?.canRequestCompact ?? false,
            compactPending: current.control?.compactPending ?? false,
            compacting: false,
            currentModeId: current.control?.currentModeId,
            activeTurnId: response.turnId,
          },
        }))
      } else if (response.kind === 'handled') {
        if (get().activeSessionId !== response.sessionId) {
          return true
        }
        if (response.message === 'compact accepted') {
          await get().refreshSessions()
          await get().switchSession(response.sessionId)
        } else if (
          !slashCommand &&
          response.message.trim() &&
          response.message !== 'command handled'
        ) {
          // 内置命令（compact/model 等）走 HTTP；扩展斜杠命令走 SSE，避免重复展示。
          set((current) => ({
            blocks: [...current.blocks, commandNoteBlock(response.message)],
          }))
        }
      }
      return true
    } catch (err) {
      console.error('submitPrompt failed:', err)
      set({
        transientHint: err instanceof Error ? err.message : '发送失败，请重试',
      })
      return false
    } finally {
      if (compactCommand) {
        const current = get()
        set({
          compactSubmitting: false,
          phase: resolvePhase(current.control, false),
        })
      }
    }
  },

  toggleComposerDeliveryMode: () => {
    set((current) => ({
      composerDeliveryMode:
        current.composerDeliveryMode === 'queued' ? 'inject' : 'queued',
    }))
  },

  injectPendingMessage: async (id: string) => {
    const state = get()
    const message = state.pendingMessages.find((item) => item.id === id)
    if (!message || !state.activeSessionId) return

    if (!canInjectMidTurn(state.control, state.compactSubmitting)) {
      set({
        transientHint: '当前 turn 已结束，无法 inject；消息会保留在 queue 中',
      })
      return
    }

    try {
      await api.injectMessage(state.activeSessionId, message.text)
      set((current) => ({
        pendingMessages: current.pendingMessages.filter(
          (item) => item.id !== id
        ),
      }))
    } catch (err) {
      console.error('injectMessage failed:', err)
      set({
        transientHint:
          err instanceof Error ? err.message : 'Inject 失败，请重试',
      })
    }
  },

  removePendingMessage: (id: string) => {
    set((current) => ({
      pendingMessages: current.pendingMessages.filter((item) => item.id !== id),
    }))
  },

  restorePendingMessage: (id: string) => {
    const state = get()
    const message = state.pendingMessages.find((item) => item.id === id)
    if (!message) return null
    get().removePendingMessage(id)
    return message.text
  },

  flushPendingQueued: async () => {
    const state = get()
    const { activeSessionId, phase, compactSubmitting } = state
    if (!activeSessionId) return
    if (isExecutionPhase(phase, compactSubmitting)) return

    const normalized = state.pendingMessages.map((item) =>
      item.delivery === 'inject'
        ? { ...item, delivery: 'queued' as const }
        : item
    )
    const toFlush = normalized.filter((item) => item.delivery === 'queued')
    if (toFlush.length === 0) return

    set({ pendingMessages: [] })

    for (const message of toFlush) {
      try {
        await api.submitPrompt(activeSessionId, message.text)
      } catch (err) {
        console.error('flushPendingQueued failed:', err)
        set((current) => ({
          pendingMessages: [...current.pendingMessages, message],
          transientHint:
            err instanceof Error ? err.message : '排队消息发送失败',
        }))
        break
      }
    }
  },

  abortCurrentTurn: async () => {
    const { activeSessionId } = get()
    if (!activeSessionId) return
    await api.abortSession(activeSessionId)
  },

  applyDelta: (delta: ConversationDelta) => {
    applyDeltaToState(get(), delta, get, set)
  },

  clearTransientHint: () => {
    set({ transientHint: null })
  },
}))
