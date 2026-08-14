import {
  AGENT_SESSION_STATUSES,
  APPROVAL_DECISIONS,
  BLOCK_STATUSES,
  PHASES,
  TOOL_CALL_STATUSES,
  TOOL_OUTPUT_STREAMS,
} from './types'
import type {
  AgentSessionLink,
  AgentSessionUpdate,
  ConversationBlock,
  ConversationControlState,
  ConversationCursor,
  ConversationDelta,
  ConversationSnapshot,
  ConversationStreamEnvelope,
  PendingAskUserQuestion,
  PendingAskUserQuestionsResponse,
  PromptAttachmentWire,
  ToolApproval,
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

function requiredString(source: JsonObject, name: string): string {
  const value = source[name]
  if (typeof value !== 'string')
    throw new ProtocolDecodeError(`expected string ${name}`)
  return value
}

function optionalString(source: JsonObject, name: string): string | undefined {
  const value = source[name]
  if (value == null) return undefined
  if (typeof value !== 'string')
    throw new ProtocolDecodeError(`expected string ${name}`)
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
  if (value == null) return undefined
  if (!isObject(value)) throw new ProtocolDecodeError(`expected object ${name}`)
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
  if (typeof value !== 'number') {
    throw new ProtocolDecodeError(`expected number ${name}`)
  }
  return value
}

function decodeObject(value: unknown, context: string): JsonObject {
  if (!isObject(value))
    throw new ProtocolDecodeError(`expected object ${context}`)
  return value
}

function stringEnumDecoder<const Values extends readonly string[]>(
  context: string,
  values: Values
): (value: unknown) => Values[number] {
  const members = new Set<string>(values)
  return (value) => {
    if (typeof value === 'string' && members.has(value)) {
      return value as Values[number]
    }
    throw new ProtocolDecodeError(`invalid ${context} ${String(value)}`)
  }
}

const decodePhase = stringEnumDecoder('phase', PHASES)
const decodeBlockStatus = stringEnumDecoder('block status', BLOCK_STATUSES)
const decodeToolCallStatus = stringEnumDecoder(
  'tool call status',
  TOOL_CALL_STATUSES
)
const decodeToolOutputStream = stringEnumDecoder(
  'tool output stream',
  TOOL_OUTPUT_STREAMS
)
const decodeAgentSessionStatus = stringEnumDecoder(
  'agent session status',
  AGENT_SESSION_STATUSES
)
const decodeApprovalDecision = stringEnumDecoder(
  'approval decision',
  APPROVAL_DECISIONS
)

function decodeToolApproval(value: unknown): ToolApproval {
  const object = decodeObject(value, 'tool approval')
  return {
    callId: requiredString(object, 'callId'),
    prompt: requiredString(object, 'prompt'),
    ruleKey: optionalString(object, 'ruleKey'),
  }
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
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        attachments: arrayField(object, 'attachments').map(
          decodePromptAttachmentWire
        ),
      }
    }
    case 'assistant':
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        reasoningContent: optionalString(object, 'reasoningContent'),
        storageSeq: optionalNumber(object, 'storageSeq'),
        status: decodeBlockStatus(object.status),
      }
    case 'toolCall': {
      const metadata = optionalObject(object, 'metadata')
      const approval =
        object.approval == null
          ? undefined
          : decodeToolApproval(object.approval)
      const argumentsJson = optionalObject(object, 'argumentsJson')
      return {
        kind,
        id,
        name: requiredString(object, 'name'),
        arguments: requiredString(object, 'arguments'),
        text: requiredString(object, 'text'),
        status: decodeToolCallStatus(object.status),
        ...(metadata ? { metadata } : {}),
        ...(approval ? { approval } : {}),
        ...(argumentsJson ? { argumentsJson } : {}),
      }
    }
    case 'error':
      return { kind, id, message: requiredString(object, 'message') }
    case 'recap':
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        source: requiredString(object, 'source'),
      }
    case 'systemNote':
      return { kind, id, text: requiredString(object, 'text') }
    case 'compactSummary':
      return {
        kind,
        id,
        summary: requiredString(object, 'summary'),
        trigger: requiredString(object, 'trigger'),
        preTokens: requiredNumber(object, 'preTokens'),
        postTokens: requiredNumber(object, 'postTokens'),
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
  const retryStatus = optionalObject(object, 'retryStatus')
  return {
    phase: decodePhase(object.phase),
    canSubmitPrompt: requiredBoolean(object, 'canSubmitPrompt'),
    canRequestCompact: requiredBoolean(object, 'canRequestCompact'),
    activeTurnId: optionalString(object, 'activeTurnId'),
    retryStatus: retryStatus
      ? {
          status: optionalNumber(retryStatus, 'status'),
          attempt: requiredNumber(retryStatus, 'attempt'),
          maxRetries: requiredNumber(retryStatus, 'maxRetries'),
          delayMs: requiredNumber(retryStatus, 'delayMs'),
        }
      : undefined,
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
    case 'resetBlock':
      return { kind, blockId: requiredString(object, 'blockId') }
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
        argumentsJson: optionalObject(object, 'argumentsJson'),
      }
    case 'agentSessionUpdated':
      return {
        kind,
        agentSession: decodeAgentSessionUpdate(object.agentSession),
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
    case 'customEvent':
      return {
        kind,
        extensionId: requiredString(object, 'extensionId'),
        eventType: requiredString(object, 'eventType'),
        schemaVersion: requiredNumber(object, 'schemaVersion'),
        payload: object.payload,
      }
    case 'toolApprovalRequested':
      return {
        kind,
        approval: decodeToolApproval(object.approval),
      }
    case 'toolApprovalResolved':
      return {
        kind,
        callId: requiredString(object, 'callId'),
        decision: decodeApprovalDecision(object.decision),
      }
    default:
      throw new ProtocolDecodeError(`invalid delta kind ${kind}`)
  }
}

function decodeAskUserOption(value: unknown) {
  const object = decodeObject(value, 'ask-user option')
  return {
    label: requiredString(object, 'label'),
    description: requiredString(object, 'description'),
    preview: optionalString(object, 'preview'),
    recommended:
      object.recommended === undefined
        ? undefined
        : requiredBoolean(object, 'recommended'),
  }
}

function decodeAskUserQuestion(value: unknown) {
  const object = decodeObject(value, 'ask-user question')
  return {
    question: requiredString(object, 'question'),
    header: requiredString(object, 'header'),
    options: arrayField(object, 'options').map(decodeAskUserOption),
    multiSelect:
      object.multiSelect === undefined
        ? undefined
        : requiredBoolean(object, 'multiSelect'),
  }
}

export function decodePendingAskUserQuestion(
  value: unknown
): PendingAskUserQuestion {
  const object = decodeObject(value, 'pending ask-user question')
  const metadata = optionalObject(object, 'metadata')
  return {
    sessionId: requiredString(object, 'sessionId'),
    callId: requiredString(object, 'callId'),
    questions: arrayField(object, 'questions').map(decodeAskUserQuestion),
    autoSelectAt: optionalNumber(object, 'autoSelectAt'),
    metadata: metadata
      ? { source: optionalString(metadata, 'source') }
      : undefined,
    serverTime: optionalNumber(object, 'serverTime'),
    receivedAtMonotonic: performance.now(),
  }
}

export function decodePendingAskUserQuestionsResponse(
  value: unknown
): PendingAskUserQuestionsResponse {
  const object = decodeObject(value, 'pending ask-user questions response')
  return {
    questions: arrayField(object, 'questions').map(
      decodePendingAskUserQuestion
    ),
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

export function decodeConversationSnapshot(
  value: unknown
): ConversationSnapshot {
  const object = decodeObject(value, 'conversation snapshot')
  return {
    sessionId: requiredString(object, 'sessionId'),
    sessionTitle: requiredString(object, 'sessionTitle'),
    cursor: decodeConversationCursor(object.cursor),
    control: decodeConversationControlState(object.control),
    blocks: arrayField(object, 'blocks').map(decodeConversationBlock),
    agentSessions: arrayField(object, 'agentSessions').map(
      decodeAgentSessionLink
    ),
  }
}

function decodeAgentSessionLink(value: unknown): AgentSessionLink {
  const object = decodeObject(value, 'agent session link')
  return {
    childSessionId: requiredString(object, 'childSessionId'),
    toolCallId: optionalString(object, 'toolCallId'),
    agentName: requiredString(object, 'agentName'),
    task: requiredString(object, 'task'),
    status: decodeAgentSessionStatus(object.status),
    finalSessionId: optionalString(object, 'finalSessionId'),
    summary: optionalString(object, 'summary'),
    error: optionalString(object, 'error'),
  }
}

function decodeAgentSessionUpdate(value: unknown): AgentSessionUpdate {
  const object = decodeObject(value, 'agent session update')
  const kind = requiredString(object, 'kind')
  const childSessionId = requiredString(object, 'childSessionId')
  switch (kind) {
    case 'spawned':
      return {
        kind,
        childSessionId,
        toolCallId: optionalString(object, 'toolCallId'),
        agentName: requiredString(object, 'agentName'),
        task: requiredString(object, 'task'),
      }
    case 'completed':
      return {
        kind,
        childSessionId,
        finalSessionId: requiredString(object, 'finalSessionId'),
        summary: requiredString(object, 'summary'),
      }
    case 'failed':
      return {
        kind,
        childSessionId,
        finalSessionId: requiredString(object, 'finalSessionId'),
        error: requiredString(object, 'error'),
      }
    case 'progress':
      return {
        kind,
        childSessionId,
        phase: decodePhase(object.phase),
        currentTool: optionalString(object, 'currentTool'),
      }
    default:
      throw new ProtocolDecodeError(`invalid agent session update kind ${kind}`)
  }
}
