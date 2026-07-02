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
          last.argumentsJson = delta.argumentsJson
        } else {
          result.push({
            kind: 'patchArguments',
            blockId: delta.blockId,
            arguments: delta.arguments,
            argumentsJson: delta.argumentsJson,
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

function findBlockIdx(
  blocks: ConversationBlock[],
  mutations: Map<number, ConversationBlock>,
  predicate: (block: ConversationBlock) => boolean
): number {
  const inBlocks = blocks.findIndex(predicate)
  if (inBlocks !== -1) return inBlocks
  for (const [idx, block] of mutations) {
    if (predicate(block)) return idx
  }
  return -1
}

function nextMutationIdx(
  blocks: ConversationBlock[],
  mutations: Map<number, ConversationBlock>
): number {
  let idx = blocks.length
  while (mutations.has(idx)) {
    idx += 1
  }
  return idx
}

function findOrCreateToolCallIdx(
  blocks: ConversationBlock[],
  mutations: Map<number, ConversationBlock>,
  callId: string
): number {
  const existing = findBlockIdx(
    blocks,
    mutations,
    (b) => b.kind === 'toolCall' && b.id === callId
  )
  if (existing !== -1) return existing
  const newIdx = nextMutationIdx(blocks, mutations)
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
    const idx = findBlockIdx(blocks, mutations, (b) => b.id === blockId)
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
    mutations.set(nextMutationIdx(blocks, mutations), newBlock)
    needsNewBlocks = true
    return findBlockIdx(blocks, mutations, (b) => b.id === blockId)
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
        const idx = findBlockIdx(
          blocks,
          mutations,
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
