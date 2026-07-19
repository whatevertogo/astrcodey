// HTTP wire contracts come from Rust-generated bindings. Local interfaces are
// limited to frontend state and the strictly decoded conversation/SSE model.

import type {
  AgentSessionStatusDto,
  ApprovalModeDto,
  ApplyProviderPresetRequest,
  ApplyProviderPresetResponseDto,
  AvailableModelDto,
  CommandCompletionItemDto,
  CommandCompletionResponse as CommandCompletionResponseDto,
  CommandInvokeResponse,
  CompactSessionResponse as CompactSessionResponseDto,
  ConfigViewResponseDto,
  ConversationBlockStatusDto,
  ConversationCursorDto,
  CreateSessionRequest,
  CreateSessionResponseDto,
  CurrentModelResponseDto,
  ExtensionDeclarationDto,
  ExtensionDiagnosticsDto,
  ExtensionHttpRouteDto,
  ExtensionStageDiagnosticsDto,
  ExtensionStateDto,
  KeybindingDto,
  ModelDto,
  ModelTestResponseDto,
  PhaseDto,
  ProfileDto,
  PromptAttachmentDto,
  PromptRequest,
  PromptSubmitResponse as PromptSubmitResponseDto,
  ProviderAuthSchemeDto,
  ProviderCatalogResponseDto,
  ProviderEndpointPresetDto,
  ProviderSpecCapabilitiesDto,
  ProviderSpecDto,
  ProviderWireFormatDto,
  RemoveProviderPresetRequest,
  RemoveProviderPresetResponseDto,
  SessionListItemDto,
  SessionListResponseDto,
  ShadowedSlashCommandDto,
  SlashCommandInfoDto,
  SlashCommandListResponseDto,
  StatusItemDto,
  ToolOutputStreamDto,
} from './generated'

export {
  AGENT_SESSION_STATUSES,
  APPROVAL_MODES,
  BLOCK_STATUSES,
  PHASES,
  PROVIDER_AUTH_SCHEMES,
  PROVIDER_WIRE_FORMATS,
  TOOL_OUTPUT_STREAMS,
} from './generated'

export type Phase = PhaseDto
export type ToolOutputStream = ToolOutputStreamDto
export type BlockStatus = ConversationBlockStatusDto
export type ApprovalMode = ApprovalModeDto
export type {
  ApplyProviderPresetRequest,
  CommandInvokeResponse,
  CreateSessionRequest,
  PromptRequest,
  RemoveProviderPresetRequest,
}

// ── Request/Response ──

export type CreateSessionResponse = CreateSessionResponseDto

export type PromptAttachmentWire = PromptAttachmentDto

export interface PromptAttachment {
  id: string
  file: File
  filename: string
  mediaType: string
  previewUrl: string
}

export type PromptSubmitResponse = PromptSubmitResponseDto

export type CompactSessionResponse = CompactSessionResponseDto

export type SlashCommandInfo = SlashCommandInfoDto

export type CommandCompletionItem = CommandCompletionItemDto

export type CommandCompletionResponse = CommandCompletionResponseDto

export type KeybindingInfo = KeybindingDto

export type StatusItemInfo = StatusItemDto

export type SlashCommandListResponse = SlashCommandListResponseDto

export type ShadowedSlashCommandInfo = ShadowedSlashCommandDto

// ── Session List ──

export type SessionListItem = SessionListItemDto

export type SessionListResponse = SessionListResponseDto

// ── Conversation Snapshot ──

export type AgentSessionStatus = AgentSessionStatusDto

export interface AgentSessionLink {
  childSessionId: string
  toolCallId?: string
  agentName?: string
  task?: string
  /** 省略时表示仅更新 phase/currentTool，不改动终态 status */
  status?: AgentSessionStatus
  finalSessionId?: string
  summary?: string
  error?: string
  phase?: Phase
  currentTool?: string
}

export type ConversationCursor = ConversationCursorDto

export interface ConversationControlState {
  phase: Phase
  canSubmitPrompt: boolean
  canRequestCompact: boolean
  compactPending: boolean
  compacting: boolean
  activeTurnId?: string
}

export type ConversationBlock =
  | {
      kind: 'user'
      id: string
      text: string
      attachments?: PromptAttachmentWire[]
      source?: string
    }
  | {
      kind: 'assistant'
      id: string
      text: string
      reasoningContent?: string
      status: BlockStatus
    }
  | {
      kind: 'toolCall'
      id: string
      name: string
      arguments: string
      argumentsJson?: Record<string, unknown>
      text: string
      status: BlockStatus
      metadata?: Record<string, unknown>
    }
  | { kind: 'error'; id: string; message: string }
  | { kind: 'systemNote'; id: string; text: string }
  | {
      kind: 'compactSummary'
      id: string
      summary: string
      trigger: string
      preTokens: number
      postTokens: number
      transcriptPath?: string
    }

export interface ConversationSnapshot {
  sessionId: string
  sessionTitle: string
  cursor: ConversationCursor
  phase: Phase
  control: ConversationControlState
  blocks: ConversationBlock[]
  agentSessions: AgentSessionLink[]
}

// ── SSE Stream ──

export interface ConversationStreamEnvelope {
  sessionId: string
  cursor: ConversationCursor
  delta: ConversationDelta
}

export type ConversationDelta =
  | { kind: 'appendBlock'; block: ConversationBlock }
  | { kind: 'patchBlock'; blockId: string; textDelta: string }
  | { kind: 'finalizeBlock'; block: ConversationBlock }
  | { kind: 'updateControlState'; control: ConversationControlState }
  | { kind: 'rehydrateRequired' }
  | {
      kind: 'sessionContinued'
      parentSessionId: string
      newSessionId: string
      parentCursor: ConversationCursor
    }
  | {
      kind: 'toolOutput'
      callId: string
      stream: ToolOutputStream
      delta: string
    }
  | { kind: 'thinkingDelta'; blockId: string; delta: string }
  | {
      kind: 'patchArguments'
      blockId: string
      arguments: string
      argumentsJson?: Record<string, unknown>
    }
  | { kind: 'agentSessionUpdated'; agentSession: AgentSessionLink }
  | { kind: 'agentSessionRemoved'; childSessionId: string }
  | { kind: 'statusItemUpdate'; id: string; text: string }
  | { kind: 'extensionRegistryChanged' }
  | {
      kind: 'patchToolMetadata'
      blockId: string
      metadata: Record<string, unknown>
    }
  | {
      kind: 'patchToolCall'
      blockId: string
      text: string
      metadata?: Record<string, unknown>
    }

// ── App State ──

export interface ConnectionState {
  status: 'disconnected' | 'connecting' | 'connected' | 'error'
  error?: string
}

// ── Config / Models ──

export type ProfileView = ProfileDto

export type ProviderWireFormat = ProviderWireFormatDto
export type ProviderAuthScheme = ProviderAuthSchemeDto

export type ModelView = ModelDto

export type ConfigView = ConfigViewResponseDto

export type ProviderCatalogView = ProviderCatalogResponseDto

export type ProviderSpecView = ProviderSpecDto

export type ProviderEndpointPresetView = ProviderEndpointPresetDto

export type ProviderSpecCapabilitiesView = ProviderSpecCapabilitiesDto

export type ApplyProviderPresetResponse = ApplyProviderPresetResponseDto

export type RemoveProviderPresetResponse = RemoveProviderPresetResponseDto

export type ExtensionStateView = ExtensionStateDto

export type ExtensionDeclarationView = ExtensionDeclarationDto

export type ExtensionHttpRouteView = ExtensionHttpRouteDto

export type ExtensionDiagnosticsView = ExtensionDiagnosticsDto

export type ExtensionStageDiagnosticsView = ExtensionStageDiagnosticsDto

export type CurrentModelInfo = CurrentModelResponseDto
export type AvailableModel = AvailableModelDto
export type ModelTestResult = ModelTestResponseDto
