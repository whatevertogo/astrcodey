import type {
  AgentSessionLink,
  ConversationBlock,
  ConversationControlState,
  ExtensionStateView,
  KeybindingInfo,
  PendingAskUserQuestion,
  PromptAttachment,
  SessionListItem,
  SlashCommandInfo,
} from '../services/types'
import type { SessionStreamStatus } from './sessionStreamController'

export type MessageDelivery = 'queued' | 'inject'

/** 本次提交的落地方式；`queued` 时预览 URL 的所有权转交 pending 队列。 */
export type PromptDelivery = 'sent' | 'queued' | 'injected' | 'handled'

export interface PendingMessage {
  id: string
  text: string
  delivery: MessageDelivery
  attachments: PromptAttachment[]
}

export interface ActiveSessionStream {
  stop: () => void
}

export interface AppState {
  serverPort: number | null
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error'
  connectionError: string | null

  sessions: SessionListItem[]
  unsupportedLogCount: number
  corruptLogCount: number
  /** 文件夹（workingDir）显示顺序；应用启动后首次拉取会话时排序，之后仅追加/移除。 */
  projectFolderOrder: string[]
  activeSessionId: string | null
  activeSessionTitle: string | null
  workingDir: string | null

  blocks: ConversationBlock[]
  /** Live preview block id -> owning turn id. Durable blocks never appear here. */
  transientBlockOwners: Record<string, string>
  control: ConversationControlState | null
  cursor: string | null
  timelineOlderCursor: string | null
  timelineHasOlder: boolean
  timelineLoading: boolean
  timelinePageBlockIds: string[][]
  timelineDetachedFromLatest: boolean
  compactSubmitting: boolean

  sessionStream: ActiveSessionStream | null
  sessionStreamStatus: SessionStreamStatus
  sessionStreamError: string | null
  modelRefreshKey: number
  agentSessions: AgentSessionLink[]
  statusItems: Record<string, string>
  statusItemRevisions: Record<string, number>
  keybindings: KeybindingInfo[]
  slashCommands: SlashCommandInfo[]
  extensions: ExtensionStateView[]
  transientHint: string | null
  pendingMessages: PendingMessage[]
  pendingAskUserQuestions: Record<string, PendingAskUserQuestion>
  resolvedAskUserCallIds: Record<string, string>
  pendingAskUserRefreshInFlight: boolean
  askUserEventRevision: number
  askUserExtensionAvailable: boolean | null
  composerDeliveryMode: MessageDelivery

  initServer: () => Promise<void>
  refreshSessions: () => Promise<void>
  createSession: (workingDir: string) => Promise<void>
  deleteSession: (sessionId: string) => Promise<void>
  forkSession: (sourceSessionId: string, storageSeq?: number) => Promise<void>
  deleteProject: (workingDir: string) => Promise<void>
  bumpModelRefreshKey: () => void
  switchSession: (sessionId: string) => Promise<void>
  refreshConversationSnapshot: () => Promise<string | null>
  loadOlderConversationItems: () => Promise<void>
  returnToLatestConversation: () => Promise<void>
  refreshPendingAskUserQuestions: () => Promise<void>
  refreshExtensionData: () => Promise<void>
  refreshCommands: () => Promise<void>
  executeExtensionCommand: (
    command: string,
    argumentsText?: string
  ) => Promise<boolean>
  submitPrompt: (
    text: string,
    attachments?: PromptAttachment[]
  ) => Promise<PromptDelivery | false>
  abortCurrentTurn: () => Promise<void>
  applyDelta: (delta: import('../services/types').ConversationDelta) => void
  clearTransientHint: () => void
  toggleComposerDeliveryMode: () => void
  injectPendingMessage: (id: string) => Promise<void>
  removePendingMessage: (id: string) => void
  resendPendingMessage: (id: string) => Promise<void>
  restorePendingMessage: (id: string) => PendingMessage | null
  flushPendingQueued: () => Promise<void>
}
