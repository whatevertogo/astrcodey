import type {
  ConversationBlock,
  ConversationDelta,
  ToolOutputStream,
} from '../../services/types'

export type CoalescedDelta =
  | { kind: 'patchBlock'; blockId: string; textDelta: string }
  | { kind: 'thinkingDelta'; blockId: string; delta: string }
  | {
      kind: 'patchArguments'
      blockId: string
      arguments: string
      argumentsJson?: Record<string, unknown>
    }
  | {
      kind: 'toolOutput'
      callId: string
      parts: { stream: ToolOutputStream; delta: string }[]
    }
  | { kind: 'other'; delta: ConversationDelta }

export function coalesceDeltas(deltas: ConversationDelta[]): CoalescedDelta[] {
  const result: CoalescedDelta[] = []
  for (const delta of deltas) {
    const last = result[result.length - 1]
    switch (delta.kind) {
      case 'patchBlock':
        if (last?.kind === 'patchBlock' && last.blockId === delta.blockId) {
          last.textDelta += delta.textDelta
        } else {
          result.push({
            kind: 'patchBlock',
            blockId: delta.blockId,
            textDelta: delta.textDelta,
          })
        }
        break
      case 'thinkingDelta':
        if (last?.kind === 'thinkingDelta' && last.blockId === delta.blockId) {
          last.delta += delta.delta
        } else {
          result.push({
            kind: 'thinkingDelta',
            blockId: delta.blockId,
            delta: delta.delta,
          })
        }
        break
      case 'patchArguments':
        if (last?.kind === 'patchArguments' && last.blockId === delta.blockId) {
          last.arguments = delta.arguments
          if (delta.argumentsJson) {
            last.argumentsJson = delta.argumentsJson
          } else {
            delete last.argumentsJson
          }
        } else {
          result.push({
            kind: 'patchArguments',
            blockId: delta.blockId,
            arguments: delta.arguments,
            ...(delta.argumentsJson
              ? { argumentsJson: delta.argumentsJson }
              : {}),
          })
        }
        break
      case 'toolOutput':
        if (last?.kind === 'toolOutput' && last.callId === delta.callId) {
          last.parts.push({ stream: delta.stream, delta: delta.delta })
        } else {
          result.push({
            kind: 'toolOutput',
            callId: delta.callId,
            parts: [{ stream: delta.stream, delta: delta.delta }],
          })
        }
        break
      default:
        result.push({ kind: 'other', delta })
    }
  }

  return result
}

export function applyCoalescedDeltas(
  blocks: ConversationBlock[],
  coalesced: CoalescedDelta[]
): ConversationBlock[] {
  if (coalesced.length === 0) return blocks

  const mutations = new Map<number, ConversationBlock>()
  const blockIndex = new Map<string, number>()
  const toolCallIndex = new Map<string, number>()
  blocks.forEach((block, index) => {
    if (!blockIndex.has(block.id)) {
      blockIndex.set(block.id, index)
    }
    if (block.kind === 'toolCall' && !toolCallIndex.has(block.id)) {
      toolCallIndex.set(block.id, index)
    }
  })
  let nextBlockIndex = blocks.length
  let changed = false

  const insertBlock = (block: ConversationBlock): number => {
    const index = nextBlockIndex
    nextBlockIndex += 1
    mutations.set(index, block)
    if (!blockIndex.has(block.id)) {
      blockIndex.set(block.id, index)
    }
    if (block.kind === 'toolCall') {
      toolCallIndex.set(block.id, index)
    }
    changed = true
    return index
  }

  const findOrCreateIdx = (
    blockId: string,
    kind: 'assistant' | 'toolCall'
  ): number => {
    const existing = blockIndex.get(blockId)
    if (existing !== undefined) return existing
    const newBlock: ConversationBlock =
      kind === 'assistant'
        ? { kind: 'assistant', id: blockId, text: '', status: 'streaming' }
        : {
            kind: 'toolCall',
            id: blockId,
            name: '',
            arguments: '',
            text: '',
            status: 'streaming',
          }
    return insertBlock(newBlock)
  }

  const findOrCreateToolCallIdx = (callId: string): number => {
    const existing = toolCallIndex.get(callId)
    if (existing !== undefined) return existing
    return insertBlock({
      kind: 'toolCall',
      id: callId,
      name: '',
      arguments: '',
      text: '',
      status: 'streaming',
    })
  }

  for (const c of coalesced) {
    switch (c.kind) {
      case 'patchBlock': {
        const idx = findOrCreateIdx(c.blockId, 'assistant')
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'assistant' && block.kind !== 'toolCall') break
        mutations.set(idx, { ...block, text: (block.text ?? '') + c.textDelta })
        changed = true
        break
      }
      case 'thinkingDelta': {
        const idx = findOrCreateIdx(c.blockId, 'assistant')
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'assistant') break
        mutations.set(idx, {
          ...block,
          reasoningContent: (block.reasoningContent ?? '') + c.delta,
        })
        changed = true
        break
      }
      case 'patchArguments': {
        const idx = toolCallIndex.get(c.blockId)
        if (idx === undefined) break
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'toolCall') break
        if (!c.arguments.trim()) break
        mutations.set(idx, {
          ...block,
          arguments: c.arguments,
          ...(c.argumentsJson ? { argumentsJson: c.argumentsJson } : {}),
        })
        changed = true
        break
      }
      case 'toolOutput': {
        const output = c.parts
          .map((p) => (p.stream === 'stderr' ? '\n[stderr] ' : '\n') + p.delta)
          .join('')
        const idx = findOrCreateToolCallIdx(c.callId)
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'toolCall') break
        const prefix =
          output.startsWith('\n') && !block.text ? output.slice(1) : output
        mutations.set(idx, { ...block, text: block.text + prefix })
        changed = true
        break
      }
      case 'other':
        break
    }
  }

  let newBlocks = blocks
  if (changed) {
    newBlocks = [...blocks]
    for (const [idx, block] of [...mutations.entries()].sort(
      ([left], [right]) => left - right
    )) {
      if (idx < blocks.length) {
        newBlocks[idx] = block
      } else {
        newBlocks.push(block)
      }
    }
  }

  return newBlocks
}
