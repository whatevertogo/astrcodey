import type {
  AgentSessionLink,
  AgentSessionStatus,
  ConversationBlock,
} from '../../services/types'

export function mergeBlock(
  current: ConversationBlock,
  incoming: ConversationBlock
): ConversationBlock {
  if (current.kind === 'assistant' && incoming.kind === 'assistant') {
    return {
      ...incoming,
      text: incoming.text ?? current.text,
      reasoningContent: incoming.reasoningContent ?? current.reasoningContent,
    }
  }

  if (current.kind === 'toolCall' && incoming.kind === 'toolCall') {
    const metadata = incoming.metadata ?? current.metadata
    const argumentsJson = incoming.argumentsJson ?? current.argumentsJson
    return {
      ...incoming,
      name: incoming.name.trim() ? incoming.name : current.name,
      arguments: incoming.arguments.trim()
        ? incoming.arguments
        : current.arguments,
      text: incoming.text.trim() ? incoming.text : current.text,
      ...(metadata ? { metadata } : {}),
      ...(argumentsJson ? { argumentsJson } : {}),
    }
  }

  return incoming
}

export function upsertBlock(
  blocks: ConversationBlock[],
  block: ConversationBlock
): ConversationBlock[] {
  const idx = blocks.findIndex((item) => item.id === block.id)
  if (idx === -1) return [...blocks, block]

  const next = [...blocks]
  next[idx] = mergeBlock(next[idx], block)
  return next
}

function isTerminalAgentStatus(
  status: AgentSessionStatus | undefined
): boolean {
  return status === 'completed' || status === 'failed'
}

export function mergeAgentSession(
  current: AgentSessionLink,
  incoming: AgentSessionLink
): AgentSessionLink {
  const status =
    incoming.status ??
    (isTerminalAgentStatus(current.status) ? current.status : 'running')
  const running = status === 'running'
  const phaseProvided = incoming.phase !== undefined
  const currentTool = running
    ? phaseProvided
      ? incoming.currentTool
      : (incoming.currentTool ?? current.currentTool)
    : undefined

  return {
    ...current,
    ...incoming,
    status,
    agentName: incoming.agentName?.trim()
      ? incoming.agentName
      : current.agentName,
    task: incoming.task?.trim() ? incoming.task : current.task,
    toolCallId: incoming.toolCallId ?? current.toolCallId,
    finalSessionId: incoming.finalSessionId ?? current.finalSessionId,
    summary: incoming.summary ?? current.summary,
    error: incoming.error ?? current.error,
    phase: running ? (incoming.phase ?? current.phase) : undefined,
    currentTool,
  }
}

export function commandNoteBlock(message: string): ConversationBlock {
  return {
    kind: 'systemNote',
    id: `command-${Date.now()}`,
    text: message,
  }
}

export function isCompactCommand(text: string): boolean {
  return /^\/compact(?:\s|$)/.test(text.trim())
}

export async function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  message: string
): Promise<T> {
  let timeoutId: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<never>((_, reject) => {
    timeoutId = setTimeout(() => reject(new Error(message)), timeoutMs)
  })
  try {
    return await Promise.race([promise, timeout])
  } finally {
    if (timeoutId) clearTimeout(timeoutId)
  }
}
