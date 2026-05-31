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
  const textPatches = new Map<string, string>()
  const thinkingPatches = new Map<string, string>()
  const argPatches = new Map<
    string,
    { arguments: string; argumentsJson?: Record<string, unknown> }
  >()
  const toolOutputs = new Map<
    string,
    { stream: ToolOutputStream; delta: string }[]
  >()
  const others: CoalescedDelta[] = []

  for (const delta of deltas) {
    switch (delta.kind) {
      case 'patchBlock': {
        const existing = textPatches.get(delta.blockId)
        textPatches.set(
          delta.blockId,
          existing ? existing + delta.textDelta : delta.textDelta
        )
        break
      }
      case 'thinkingDelta': {
        const existing = thinkingPatches.get(delta.blockId)
        thinkingPatches.set(
          delta.blockId,
          existing ? existing + delta.delta : delta.delta
        )
        break
      }
      case 'patchArguments': {
        argPatches.set(delta.blockId, {
          arguments: delta.arguments,
          argumentsJson: delta.argumentsJson,
        })
        break
      }
      case 'toolOutput': {
        const existing = toolOutputs.get(delta.callId)
        if (existing) {
          existing.push({ stream: delta.stream, delta: delta.delta })
        } else {
          toolOutputs.set(delta.callId, [
            { stream: delta.stream, delta: delta.delta },
          ])
        }
        break
      }
      default:
        others.push({ kind: 'other', delta })
    }
  }

  const result: CoalescedDelta[] = []

  for (const [blockId, textDelta] of textPatches) {
    result.push({ kind: 'patchBlock', blockId, textDelta })
  }
  for (const [blockId, delta] of thinkingPatches) {
    result.push({ kind: 'thinkingDelta', blockId, delta })
  }
  for (const [blockId, args] of argPatches) {
    result.push({ kind: 'patchArguments', blockId, ...args })
  }
  for (const [callId, parts] of toolOutputs) {
    result.push({ kind: 'toolOutput', callId, parts })
  }
  result.push(...others)

  return result
}

function findOrCreateToolCallIdx(
  blocks: ConversationBlock[],
  mutations: Map<number, ConversationBlock>,
  callId: string
): number {
  const inBlocks = blocks.findIndex(
    (b) => b.kind === 'toolCall' && b.id === callId
  )
  if (inBlocks !== -1) return inBlocks
  for (const [idx, block] of mutations) {
    if (block.kind === 'toolCall' && block.id === callId) {
      return idx
    }
  }
  const newIdx = blocks.length
  mutations.set(newIdx, {
    kind: 'toolCall',
    id: callId,
    name: '',
    arguments: '',
    text: '',
    status: 'streaming',
  })
  return newIdx
}

export function applyCoalescedDeltas(
  blocks: ConversationBlock[],
  coalesced: CoalescedDelta[]
): { blocks: ConversationBlock[] } {
  if (coalesced.length === 0) return { blocks }

  const mutations = new Map<number, ConversationBlock>()
  let needsNewBlocks = false

  const findOrCreateIdx = (
    blockId: string,
    kind: 'assistant' | 'toolCall'
  ): number => {
    const idx = blocks.findIndex((b) => b.id === blockId)
    if (idx !== -1) return idx
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
    mutations.set(blocks.length, newBlock)
    needsNewBlocks = true
    return blocks.length
  }

  for (const c of coalesced) {
    switch (c.kind) {
      case 'patchBlock': {
        const idx = findOrCreateIdx(c.blockId, 'assistant')
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'assistant' && block.kind !== 'toolCall') break
        mutations.set(idx, { ...block, text: (block.text ?? '') + c.textDelta })
        needsNewBlocks = true
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
        needsNewBlocks = true
        break
      }
      case 'patchArguments': {
        const idx = blocks.findIndex(
          (b) => b.kind === 'toolCall' && b.id === c.blockId
        )
        if (idx === -1) break
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'toolCall') break
        if (!c.arguments.trim()) break
        mutations.set(idx, {
          ...block,
          arguments: c.arguments,
          ...(c.argumentsJson ? { argumentsJson: c.argumentsJson } : {}),
        })
        needsNewBlocks = true
        break
      }
      case 'toolOutput': {
        const output = c.parts
          .map((p) => (p.stream === 'stderr' ? '\n[stderr] ' : '\n') + p.delta)
          .join('')
        const idx = findOrCreateToolCallIdx(blocks, mutations, c.callId)
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'toolCall') break
        const prefix =
          output.startsWith('\n') && !block.text ? output.slice(1) : output
        mutations.set(idx, { ...block, text: block.text + prefix })
        needsNewBlocks = true
        break
      }
      case 'other':
        break
    }
  }

  let newBlocks = blocks
  if (needsNewBlocks) {
    if (mutations.has(blocks.length)) {
      newBlocks = [...blocks]
      for (const [idx, block] of mutations) {
        if (idx < blocks.length) {
          newBlocks[idx] = block
        } else {
          newBlocks.push(block)
        }
      }
    } else {
      newBlocks = [...blocks]
      for (const [idx, block] of mutations) {
        newBlocks[idx] = block
      }
    }
  }

  return { blocks: newBlocks }
}

export function isDeferrableDelta(delta: ConversationDelta): boolean {
  return (
    delta.kind === 'patchBlock' ||
    delta.kind === 'thinkingDelta' ||
    delta.kind === 'patchArguments' ||
    delta.kind === 'toolOutput'
  )
}
