import type { ConversationBlock } from '../services/types'

export const MAX_TIMELINE_PAGES = 8
export const MAX_TIMELINE_BLOCKS = 1_300

export interface TimelineWindow {
  blocks: ConversationBlock[]
  pageBlockIds: string[][]
  detachedFromLatest: boolean
}

export function earliestConversationCursor(left: string, right: string) {
  return BigInt(left) <= BigInt(right) ? left : right
}

export function latestTimelineWindow(
  durable: ConversationBlock[],
  transient: ConversationBlock[]
): TimelineWindow {
  const blocks = mergeById(durable, transient)
  return {
    blocks,
    pageBlockIds: [durable.map((block) => block.id)],
    detachedFromLatest: false,
  }
}

export function prependTimelinePage(
  current: TimelineWindow,
  older: ConversationBlock[]
): TimelineWindow {
  const currentIds = new Set(current.blocks.map((block) => block.id))
  const uniqueOlder = older.filter((block) => !currentIds.has(block.id))
  const pageBlockIds = [
    uniqueOlder.map((block) => block.id),
    ...current.pageBlockIds,
  ]
  const blocks = [...uniqueOlder, ...current.blocks]

  if (pageBlockIds.length <= MAX_TIMELINE_PAGES) {
    return {
      blocks,
      pageBlockIds,
      detachedFromLatest: current.detachedFromLatest,
    }
  }

  pageBlockIds.pop()
  const retainedIds = new Set(pageBlockIds.flat())
  return {
    blocks: blocks.filter((block) => retainedIds.has(block.id)),
    pageBlockIds,
    detachedFromLatest: true,
  }
}

function mergeById(
  stable: ConversationBlock[],
  updates: ConversationBlock[]
): ConversationBlock[] {
  const merged = [...stable]
  for (const update of updates) {
    const index = merged.findIndex((block) => block.id === update.id)
    if (index === -1) {
      merged.push(update)
    } else {
      merged[index] = update
    }
  }
  return merged
}
