import type {
  AgentSessionLink,
  AgentSessionStatus,
  ApplyProviderPresetResponse,
  AvailableModel,
  CompactSessionResponse,
  CommandCompletionResponse,
  CommandInvokeResponse,
  ConfigView,
  ConversationBlock,
  ConversationControlState,
  ConversationCursor,
  ConversationDelta,
  ConversationSnapshot,
  ConversationStreamEnvelope,
  CreateSessionResponse,
  CurrentModelInfo,
  ExtensionStateView,
  ModelTestResult,
  ModelView,
  Phase,
  ProfileView,
  ProviderCatalogView,
  ProviderEndpointPresetView,
  ProviderSpecCapabilitiesView,
  ProviderSpecView,
  ProviderAuthScheme,
  ProviderWireFormat,
  PromptAttachmentWire,
  PromptSubmitResponse,
  RemoveProviderPresetResponse,
  SlashCommandInfo,
  SlashCommandListResponse,
  KeybindingInfo,
  StatusItemInfo,
  ShadowedSlashCommandInfo,
  SessionListItem,
  SessionListResponse,
  ToolOutputStream,
} from './types'

type JsonObject = Record<string, unknown>

export class ProtocolDecodeError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ProtocolDecodeError'
  }
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function arrayField(source: JsonObject, name: string): unknown[] {
  const value = source[name]
  if (!Array.isArray(value))
    throw new ProtocolDecodeError(`expected array ${name}`)
  return value
}

/** 缺省或 `null` 视为 `[]`（与 serde `skip_serializing_if` 省略字段对齐）。 */
function optionalArrayField(source: JsonObject, name: string): unknown[] {
  const value = source[name]
  if (value == null) return []
  if (!Array.isArray(value))
    throw new ProtocolDecodeError(`expected array ${name}`)
  return value
}

function requiredString(source: JsonObject, name: string): string {
  const value = source[name]
  if (typeof value !== 'string')
    throw new ProtocolDecodeError(`expected string ${name}`)
  return value
}

function optionalString(source: JsonObject, name: string): string | undefined {
  const value = source[name]
  if (value == null || typeof value !== 'string') return undefined
  return value
}

function requiredBoolean(source: JsonObject, name: string): boolean {
  const value = source[name]
  if (typeof value !== 'boolean') {
    throw new ProtocolDecodeError(`expected boolean ${name}`)
  }
  return value
}

function optionalObject(
  source: JsonObject,
  name: string
): Record<string, unknown> | undefined {
  const value = source[name]
  if (value == null || typeof value !== 'object' || Array.isArray(value))
    return undefined
  return value as Record<string, unknown>
}

function optionalNumber(source: JsonObject, name: string): number | undefined {
  const value = source[name]
  if (value == null) return undefined
  if (typeof value !== 'number') {
    throw new ProtocolDecodeError(`expected number ${name}`)
  }
  return value
}

function requiredNumber(source: JsonObject, name: string): number {
  const value = source[name]
  if (typeof value !== 'number')
    throw new ProtocolDecodeError(`expected number ${name}`)
  return value
}

function decodeObject(value: unknown, context: string): JsonObject {
  if (!isObject(value))
    throw new ProtocolDecodeError(`expected object ${context}`)
  return value
}

function decodePhase(value: unknown): Phase {
  if (
    value === 'idle' ||
    value === 'thinking' ||
    value === 'streaming' ||
    value === 'calling_tool' ||
    value === 'compacting' ||
    value === 'error'
  ) {
    return value
  }
  throw new ProtocolDecodeError(`invalid phase ${String(value)}`)
}

function decodeBlockStatus(value: unknown): 'streaming' | 'complete' | 'error' {
  if (value === 'streaming' || value === 'complete' || value === 'error') {
    return value
  }
  throw new ProtocolDecodeError(`invalid block status ${String(value)}`)
}

function decodeToolOutputStream(value: unknown): ToolOutputStream {
  if (value === 'stdout' || value === 'stderr') return value
  throw new ProtocolDecodeError(`invalid tool output stream ${String(value)}`)
}

export function decodeConversationCursor(value: unknown): ConversationCursor {
  const object = decodeObject(value, 'cursor')
  return { value: requiredString(object, 'value') }
}

function decodePromptAttachmentWire(value: unknown): PromptAttachmentWire {
  const object = decodeObject(value, 'prompt attachment')
  return {
    filename: requiredString(object, 'filename'),
    content: requiredString(object, 'content'),
    mediaType: requiredString(object, 'mediaType'),
  }
}

export function decodeConversationBlock(value: unknown): ConversationBlock {
  const object = decodeObject(value, 'conversation block')
  const kind = requiredString(object, 'kind')
  const id = requiredString(object, 'id')

  switch (kind) {
    case 'user': {
      const rawAttachments = optionalArrayField(object, 'attachments')
      const attachments =
        rawAttachments.length > 0
          ? rawAttachments.map(decodePromptAttachmentWire)
          : undefined
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        attachments,
        source: optionalString(object, 'source'),
      }
    }
    case 'assistant':
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        reasoningContent: optionalString(object, 'reasoningContent'),
        status: decodeBlockStatus(object.status),
      }
    case 'toolCall':
      return {
        kind,
        id,
        name: requiredString(object, 'name'),
        arguments: requiredString(object, 'arguments'),
        text: requiredString(object, 'text'),
        status: decodeBlockStatus(object.status),
        metadata: optionalObject(object, 'metadata'),
        argumentsJson:
          object.argumentsJson && typeof object.argumentsJson === 'object'
            ? (object.argumentsJson as Record<string, unknown>)
            : undefined,
      }
    case 'error':
      return { kind, id, message: requiredString(object, 'message') }
    case 'systemNote':
      return { kind, id, text: requiredString(object, 'text') }
    case 'compactSummary':
      return {
        kind,
        id,
        summary: requiredString(object, 'summary'),
        trigger: requiredString(object, 'trigger'),
        preTokens: optionalNumber(object, 'preTokens') ?? 0,
        postTokens: optionalNumber(object, 'postTokens') ?? 0,
        transcriptPath: optionalString(object, 'transcriptPath'),
      }
    default:
      throw new ProtocolDecodeError(`invalid block kind ${kind}`)
  }
}

export function decodeConversationControlState(
  value: unknown
): ConversationControlState {
  const object = decodeObject(value, 'control')
  return {
    phase: decodePhase(object.phase),
    canSubmitPrompt: requiredBoolean(object, 'canSubmitPrompt'),
    canRequestCompact: requiredBoolean(object, 'canRequestCompact'),
    compactPending: requiredBoolean(object, 'compactPending'),
    compacting: requiredBoolean(object, 'compacting'),
    currentModeId: optionalString(object, 'currentModeId'),
    activeTurnId: optionalString(object, 'activeTurnId'),
  }
}

export function decodeConversationDelta(value: unknown): ConversationDelta {
  const object = decodeObject(value, 'conversation delta')
  const kind = requiredString(object, 'kind')

  switch (kind) {
    case 'appendBlock':
      return { kind, block: decodeConversationBlock(object.block) }
    case 'patchBlock':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        textDelta: requiredString(object, 'textDelta'),
      }
    case 'finalizeBlock':
      return { kind, block: decodeConversationBlock(object.block) }
    case 'updateControlState':
      return { kind, control: decodeConversationControlState(object.control) }
    case 'rehydrateRequired':
      return { kind }
    case 'sessionContinued':
      return {
        kind,
        parentSessionId: requiredString(object, 'parentSessionId'),
        newSessionId: requiredString(object, 'newSessionId'),
        parentCursor: decodeConversationCursor(object.parentCursor),
      }
    case 'toolOutput':
      return {
        kind,
        callId: requiredString(object, 'callId'),
        stream: decodeToolOutputStream(object.stream),
        delta: requiredString(object, 'delta'),
      }
    case 'thinkingDelta':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        delta: requiredString(object, 'delta'),
      }
    case 'patchArguments':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        arguments: requiredString(object, 'arguments'),
        argumentsJson:
          object.argumentsJson && typeof object.argumentsJson === 'object'
            ? (object.argumentsJson as Record<string, unknown>)
            : undefined,
      }
    case 'agentSessionUpdated':
      return {
        kind,
        agentSession: decodeAgentSessionLink(object.agentSession),
      }
    case 'agentSessionRemoved':
      return {
        kind,
        childSessionId: requiredString(object, 'childSessionId'),
      }
    case 'statusItemUpdate':
      return {
        kind,
        id: requiredString(object, 'id'),
        text: requiredString(object, 'text'),
      }
    case 'extensionRegistryChanged':
      return { kind }
    case 'patchToolMetadata':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        metadata: optionalObject(object, 'metadata') ?? {},
      }
    case 'patchToolCall':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        text: requiredString(object, 'text'),
        metadata: optionalObject(object, 'metadata'),
      }
    default:
      throw new ProtocolDecodeError(`invalid delta kind ${kind}`)
  }
}

export function decodeConversationStreamEnvelope(
  value: unknown
): ConversationStreamEnvelope {
  const object = decodeObject(value, 'conversation stream envelope')
  return {
    sessionId: requiredString(object, 'sessionId'),
    cursor: decodeConversationCursor(object.cursor),
    delta: decodeConversationDelta(object.delta),
  }
}

export function tryDecodeConversationStreamEnvelope(
  value: unknown
): ConversationStreamEnvelope | null {
  try {
    return decodeConversationStreamEnvelope(value)
  } catch {
    return null
  }
}

export function decodeCreateSessionResponse(
  value: unknown
): CreateSessionResponse {
  const object = decodeObject(value, 'create session response')
  return { sessionId: requiredString(object, 'sessionId') }
}

export function decodeSessionListResponse(value: unknown): SessionListResponse {
  const object = decodeObject(value, 'session list response')
  return {
    sessions: arrayField(object, 'sessions').map(decodeSessionListItem),
  }
}

function decodeSessionListItem(value: unknown): SessionListItem {
  const object = decodeObject(value, 'session list item')
  return {
    sessionId: requiredString(object, 'sessionId'),
    workingDir: requiredString(object, 'workingDir'),
    displayName: requiredString(object, 'displayName'),
    title: requiredString(object, 'title'),
    createdAt: requiredString(object, 'createdAt'),
    updatedAt: requiredString(object, 'updatedAt'),
    parentSessionId: optionalString(object, 'parentSessionId'),
    parentStorageSeq: optionalNumber(object, 'parentStorageSeq'),
    phase: decodePhase(object.phase),
    firstUserMessage: optionalString(object, 'firstUserMessage'),
    sourceExtension: optionalString(object, 'sourceExtension'),
  }
}

export function decodeConversationSnapshot(
  value: unknown
): ConversationSnapshot {
  const object = decodeObject(value, 'conversation snapshot')
  return {
    sessionId: requiredString(object, 'sessionId'),
    sessionTitle: requiredString(object, 'sessionTitle'),
    cursor: decodeConversationCursor(object.cursor),
    phase: decodePhase(object.phase),
    control: decodeConversationControlState(object.control),
    blocks: arrayField(object, 'blocks').map(decodeConversationBlock),
    agentSessions: arrayField(object, 'agentSessions').map(
      decodeAgentSessionLink
    ),
  }
}

function decodeAgentSessionStatus(value: unknown): AgentSessionStatus {
  if (value === 'running' || value === 'completed' || value === 'failed')
    return value
  return 'running'
}

function decodeAgentSessionLink(value: unknown): AgentSessionLink {
  const object = decodeObject(value, 'agent session link')
  return {
    childSessionId: requiredString(object, 'childSessionId'),
    toolCallId: optionalString(object, 'toolCallId'),
    agentName: optionalString(object, 'agentName'),
    task: optionalString(object, 'task'),
    status:
      object.status == null
        ? undefined
        : decodeAgentSessionStatus(object.status),
    finalSessionId: optionalString(object, 'finalSessionId'),
    summary: optionalString(object, 'summary'),
    error: optionalString(object, 'error'),
    phase: object.phase == null ? undefined : decodePhase(object.phase),
    currentTool: optionalString(object, 'currentTool'),
  }
}

export function decodePromptSubmitResponse(
  value: unknown
): PromptSubmitResponse {
  const object = decodeObject(value, 'prompt submit response')
  const kind = requiredString(object, 'kind')
  switch (kind) {
    case 'accepted':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        turnId: requiredString(object, 'turnId'),
        branchedFromSessionId: optionalString(object, 'branchedFromSessionId'),
      }
    case 'handled':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        message: requiredString(object, 'message'),
      }
    default:
      throw new ProtocolDecodeError(`invalid prompt response kind ${kind}`)
  }
}

export function decodeCompactSessionResponse(
  value: unknown
): CompactSessionResponse {
  const object = decodeObject(value, 'compact session response')
  return {
    accepted: requiredBoolean(object, 'accepted'),
    deferred: requiredBoolean(object, 'deferred'),
    newSessionId: optionalString(object, 'newSessionId'),
    message: requiredString(object, 'message'),
  }
}

function decodeSlashCommandInfo(value: unknown): SlashCommandInfo {
  const object = decodeObject(value, 'slash command info')
  return {
    name: requiredString(object, 'name'),
    description: requiredString(object, 'description'),
    needsArgument: requiredBoolean(object, 'needsArgument'),
    requiresIdle: requiredBoolean(object, 'requiresIdle'),
    argumentCompletions: requiredBoolean(object, 'argumentCompletions'),
    priority: optionalNumber(object, 'priority') ?? 0,
    source: requiredString(object, 'source'),
  }
}

function decodeShadowedSlashCommandInfo(
  value: unknown
): ShadowedSlashCommandInfo {
  const object = decodeObject(value, 'shadowed slash command info')
  return {
    name: requiredString(object, 'name'),
    activeSource: requiredString(object, 'activeSource'),
    activePriority: optionalNumber(object, 'activePriority') ?? 0,
    shadowedSource: requiredString(object, 'shadowedSource'),
    shadowedPriority: optionalNumber(object, 'shadowedPriority') ?? 0,
    shadowedExtensionId: requiredString(object, 'shadowedExtensionId'),
  }
}

function decodeKeybindingInfo(value: unknown): KeybindingInfo {
  const object = decodeObject(value, 'keybinding info')
  return {
    key: requiredString(object, 'key'),
    command: requiredString(object, 'command'),
    arguments: optionalString(object, 'arguments') ?? '',
    description: requiredString(object, 'description'),
  }
}

function decodeStatusItemInfo(value: unknown): StatusItemInfo {
  const object = decodeObject(value, 'status item info')
  return {
    id: requiredString(object, 'id'),
    text: requiredString(object, 'text'),
    priority: optionalNumber(object, 'priority') ?? 0,
  }
}

export function decodeSlashCommandListResponse(
  value: unknown
): SlashCommandListResponse {
  const object = decodeObject(value, 'slash command list response')
  return {
    commands: arrayField(object, 'commands').map(decodeSlashCommandInfo),
    shadowedCommands: optionalArrayField(object, 'shadowedCommands').map(
      decodeShadowedSlashCommandInfo
    ),
    keybindings: ((object.keybindings as unknown[]) ?? []).map(
      decodeKeybindingInfo
    ),
    statusItems: ((object.statusItems as unknown[]) ?? []).map(
      decodeStatusItemInfo
    ),
  }
}

export function decodeCommandInvokeResponse(
  value: unknown
): CommandInvokeResponse {
  const object = decodeObject(value, 'command invoke response')
  const kind = requiredString(object, 'kind')
  switch (kind) {
    case 'display':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        content: requiredString(object, 'content'),
        isError: requiredBoolean(object, 'isError'),
      }
    case 'handled':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        message: requiredString(object, 'message'),
      }
    case 'started':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        turnId: requiredString(object, 'turnId'),
      }
    default:
      throw new ProtocolDecodeError(`invalid command response kind ${kind}`)
  }
}

export function decodeCommandCompletionResponse(
  value: unknown
): CommandCompletionResponse {
  const object = decodeObject(value, 'command completion response')
  return {
    items: arrayField(object, 'items').map((item) => {
      const itemObject = decodeObject(item, 'command completion item')
      return {
        label: requiredString(itemObject, 'label'),
        insertText: requiredString(itemObject, 'insertText'),
        detail: optionalString(itemObject, 'detail'),
      }
    }),
    truncated: requiredBoolean(object, 'truncated'),
  }
}

export function decodeDeleteProjectResponse(value: unknown): {
  deletedCount: number
} {
  const object = decodeObject(value, 'delete project response')
  const deletedCount = optionalNumber(object, 'deletedCount')
  if (deletedCount == null) {
    throw new ProtocolDecodeError('expected number deletedCount')
  }
  return { deletedCount }
}

export function decodeConfigView(value: unknown): ConfigView {
  const object = decodeObject(value, 'config view')
  const extensionStates: Record<string, boolean> = (() => {
    const raw = object['extensionStates']
    if (raw == null || typeof raw !== 'object' || Array.isArray(raw)) return {}
    return Object.fromEntries(
      Object.entries(raw as Record<string, unknown>)
        .filter(([, v]) => typeof v === 'boolean')
        .map(([k, v]) => [k, v as boolean])
    )
  })()
  return {
    configPath: requiredString(object, 'configPath'),
    activeProfile: requiredString(object, 'activeProfile'),
    activeModel: requiredString(object, 'activeModel'),
    activeSmallProfile: optionalString(object, 'activeSmallProfile'),
    activeSmallModel: optionalString(object, 'activeSmallModel'),
    approvalMode: decodeApprovalMode(object['approvalMode']),
    extensionStates,
    profiles: arrayField(object, 'profiles').map(decodeProfileView),
    warning: optionalString(object, 'warning'),
  }
}

export function decodeProviderCatalog(value: unknown): ProviderCatalogView {
  const object = decodeObject(value, 'provider catalog')
  return {
    providers: arrayField(object, 'providers').map(decodeProviderSpec),
  }
}

export function decodeApplyProviderPresetResponse(
  value: unknown
): ApplyProviderPresetResponse {
  const object = decodeObject(value, 'apply provider preset response')
  return {
    success: requiredBoolean(object, 'success'),
    profileName: requiredString(object, 'profileName'),
    modelId: requiredString(object, 'modelId'),
    activated: requiredBoolean(object, 'activated'),
    warning: optionalString(object, 'warning'),
  }
}

export function decodeRemoveProviderPresetResponse(
  value: unknown
): RemoveProviderPresetResponse {
  const object = decodeObject(value, 'remove provider preset response')
  return {
    success: requiredBoolean(object, 'success'),
    removedProfileName: requiredString(object, 'removedProfileName'),
    activeProfile: requiredString(object, 'activeProfile'),
    activeModel: requiredString(object, 'activeModel'),
    warning: optionalString(object, 'warning'),
  }
}

function decodeApprovalMode(value: unknown): 'manual' | 'yolo' {
  return value === 'yolo' ? 'yolo' : 'manual'
}

function decodeProfileView(value: unknown): ProfileView {
  const object = decodeObject(value, 'profile view')
  return {
    name: requiredString(object, 'name'),
    providerKind: requiredString(object, 'providerKind'),
    wireFormat: decodeProviderWireFormat(object['wireFormat']),
    authScheme: decodeProviderAuthScheme(object['authScheme']),
    baseUrl: requiredString(object, 'baseUrl'),
    hasApiKey: requiredBoolean(object, 'hasApiKey'),
    models: arrayField(object, 'models').map(decodeModelView),
  }
}

function decodeProviderWireFormat(value: unknown): ProviderWireFormat {
  switch (value) {
    case 'openai_chat_completions':
    case 'openai_responses':
    case 'anthropic_messages':
    case 'google_genai':
      return value
    default:
      throw new Error(`Invalid provider wire format: ${String(value)}`)
  }
}

function decodeProviderAuthScheme(value: unknown): ProviderAuthScheme {
  switch (value) {
    case 'none':
    case 'bearer':
    case 'x_api_key':
    case 'x_goog_api_key':
      return value
    default:
      throw new Error(`Invalid provider auth scheme: ${String(value)}`)
  }
}

function decodeProviderSpec(value: unknown): ProviderSpecView {
  const object = decodeObject(value, 'provider spec')
  return {
    id: requiredString(object, 'id'),
    displayName: requiredString(object, 'displayName'),
    providerKind: requiredString(object, 'providerKind'),
    wireFormat: decodeProviderWireFormat(object['wireFormat']),
    authScheme: decodeProviderAuthScheme(object['authScheme']),
    defaultModel: requiredString(object, 'defaultModel'),
    apiKeyEnvVars: arrayField(object, 'apiKeyEnvVars').map((item) => {
      if (typeof item !== 'string') {
        throw new ProtocolDecodeError('expected string apiKeyEnvVars item')
      }
      return item
    }),
    endpoints: arrayField(object, 'endpoints').map(
      decodeProviderEndpointPreset
    ),
    capabilities: decodeProviderSpecCapabilities(object['capabilities']),
  }
}

function decodeProviderEndpointPreset(
  value: unknown
): ProviderEndpointPresetView {
  const object = decodeObject(value, 'provider endpoint preset')
  return {
    id: requiredString(object, 'id'),
    label: requiredString(object, 'label'),
    baseUrl: optionalString(object, 'baseUrl'),
    isDefault: requiredBoolean(object, 'isDefault'),
  }
}

function decodeProviderSpecCapabilities(
  value: unknown
): ProviderSpecCapabilitiesView {
  const object = decodeObject(value, 'provider spec capabilities')
  return {
    promptCacheKey: requiredBoolean(object, 'promptCacheKey'),
    streamUsage: requiredBoolean(object, 'streamUsage'),
    reasoningEffort: requiredBoolean(object, 'reasoningEffort'),
  }
}

function decodeModelView(value: unknown): ModelView {
  const object = decodeObject(value, 'model view')
  return {
    id: requiredString(object, 'id'),
    maxTokens: optionalNumber(object, 'maxTokens'),
    contextLimit: optionalNumber(object, 'contextLimit'),
  }
}

export function decodeConfigReloadResponse(value: unknown): {
  activeProfile: string
  activeModel: string
  activeSmallProfile?: string
  activeSmallModel?: string
} {
  const object = decodeObject(value, 'config reload response')
  return {
    activeProfile: requiredString(object, 'activeProfile'),
    activeModel: requiredString(object, 'activeModel'),
    activeSmallProfile: optionalString(object, 'activeSmallProfile'),
    activeSmallModel: optionalString(object, 'activeSmallModel'),
  }
}

export function decodeActiveSelectionResponse(value: unknown): {
  success: boolean
  warning?: string
} {
  const object = decodeObject(value, 'active selection response')
  return {
    success: requiredBoolean(object, 'success'),
    warning: optionalString(object, 'warning'),
  }
}

export function decodeCurrentModelInfo(value: unknown): CurrentModelInfo {
  const object = decodeObject(value, 'current model info')
  return {
    profileName: requiredString(object, 'profileName'),
    modelId: requiredString(object, 'modelId'),
    providerKind: requiredString(object, 'providerKind'),
    wireFormat: decodeProviderWireFormat(object['wireFormat']),
  }
}

export function decodeAvailableModels(value: unknown): AvailableModel[] {
  const object = decodeObject(value, 'model list response')
  return arrayField(object, 'models').map(decodeAvailableModel)
}

function decodeAvailableModel(value: unknown): AvailableModel {
  const object = decodeObject(value, 'available model')
  return {
    profileName: requiredString(object, 'profileName'),
    modelId: requiredString(object, 'modelId'),
    providerKind: requiredString(object, 'providerKind'),
    wireFormat: decodeProviderWireFormat(object['wireFormat']),
  }
}

export function decodeModelTestResult(value: unknown): ModelTestResult {
  const object = decodeObject(value, 'model test result')
  return {
    success: requiredBoolean(object, 'success'),
    message: requiredString(object, 'message'),
  }
}

function decodeExtensionStateView(value: unknown): ExtensionStateView {
  const object = decodeObject(value, 'extension state')
  const source = requiredString(object, 'source')
  const declaration =
    object.declaration == null
      ? undefined
      : decodeExtensionDeclarationView(object.declaration)
  const diagnostics =
    object.diagnostics == null
      ? undefined
      : decodeExtensionDiagnosticsView(object.diagnostics)
  return {
    extensionId: requiredString(object, 'extensionId'),
    enabled: requiredBoolean(object, 'enabled'),
    loaded: requiredBoolean(object, 'loaded'),
    source:
      source === 'builtin' || source === 'disk' || source === 'unknown'
        ? source
        : 'unknown',
    declaration,
    diagnostics,
  }
}

function decodeExtensionDeclarationView(
  value: unknown
): NonNullable<ExtensionStateView['declaration']> {
  const object = decodeObject(value, 'extension declaration')
  return {
    id: requiredString(object, 'id'),
    capabilities: arrayField(object, 'capabilities').filter(
      (item): item is string => typeof item === 'string'
    ),
    tools: arrayField(object, 'tools').filter(isRecord),
    dynamicTools: object.dynamicTools === true,
    commands: arrayField(object, 'commands').filter(isRecord),
    dynamicCommands: object.dynamicCommands === true,
    keybindings: arrayField(object, 'keybindings').filter(isRecord),
    statusItems: arrayField(object, 'statusItems').filter(isRecord),
    events: arrayField(object, 'events').filter(isRecord),
    httpRoutes: optionalArrayField(object, 'httpRoutes').map((route) => {
      const routeObject = decodeObject(route, 'extension HTTP route')
      return {
        method: requiredString(routeObject, 'method'),
        path: requiredString(routeObject, 'path'),
        description: requiredString(routeObject, 'description'),
        maxBodyBytes: requiredNumber(routeObject, 'maxBodyBytes'),
      }
    }),
  }
}

function decodeExtensionDiagnosticsView(
  value: unknown
): NonNullable<ExtensionStateView['diagnostics']> {
  const object = decodeObject(value, 'extension diagnostics')
  return {
    load: decodeExtensionStageDiagnosticsView(object.load),
    register: decodeExtensionStageDiagnosticsView(object.register),
    start: decodeExtensionStageDiagnosticsView(object.start),
    hookCalls: optionalNumber(object, 'hookCalls') ?? 0,
    hookTimeouts: optionalNumber(object, 'hookTimeouts') ?? 0,
    lastHook: optionalString(object, 'lastHook'),
    lastDurationMs: optionalNumber(object, 'lastDurationMs'),
    lastError: optionalString(object, 'lastError'),
  }
}

function decodeExtensionStageDiagnosticsView(
  value: unknown
): NonNullable<ExtensionStateView['diagnostics']>['load'] {
  const object =
    value == null ? {} : decodeObject(value, 'extension stage diagnostics')
  return {
    status: decodeExtensionStageStatus(optionalString(object, 'status')),
    durationMs: optionalNumber(object, 'durationMs'),
    error: optionalString(object, 'error'),
  }
}

function decodeExtensionStageStatus(
  value: string | undefined
): NonNullable<ExtensionStateView['diagnostics']>['load']['status'] {
  switch (value) {
    case 'running':
    case 'succeeded':
    case 'failed':
    case 'skipped':
      return value
    default:
      return 'unknown'
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value != null && !Array.isArray(value)
}

export function decodeExtensionListResponse(value: unknown): {
  extensions: ExtensionStateView[]
} {
  const object = decodeObject(value, 'extension list response')
  return {
    extensions: arrayField(object, 'extensions').map(decodeExtensionStateView),
  }
}

export function decodeExtensionReloadResponse(value: unknown): {
  reloadErrors: string[]
} {
  const object = decodeObject(value, 'extension reload response')
  return {
    reloadErrors: arrayField(object, 'reloadErrors').map((item) =>
      typeof item === 'string' ? item : String(item)
    ),
  }
}

export function decodeSetExtensionEnabledResponse(value: unknown): {
  success: boolean
  reloadErrors: string[]
} {
  const object = decodeObject(value, 'set extension enabled response')
  return {
    success: requiredBoolean(object, 'success'),
    reloadErrors: arrayField(object, 'reloadErrors').map((item) =>
      typeof item === 'string' ? item : String(item)
    ),
  }
}
