import { memo, useRef, useMemo } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'
import { emptyStateSurface } from '../../lib/styles'
import AssistantMessage from './AssistantMessage'
import UserMessage from './UserMessage'
import ToolCallBlock from './ToolCallBlock'
import ErrorBlock from './ErrorBlock'
import SystemNote from './SystemNote'
import CompactSummaryCard from './CompactSummaryCard'
import {
  useFollowLatestScroll,
  type FollowLatestVirtualizer,
} from './useFollowLatestScroll'

interface MessageListProps {
  blocks: ConversationBlock[]
  sessionId: string | null
}

function isAssistantLike(block: ConversationBlock): boolean {
  return block.kind === 'assistant' || block.kind === 'toolCall'
}

const BlockRenderer = memo(function BlockRenderer({
  block,
  prevBlock,
  sessionId,
}: {
  block: ConversationBlock
  prevBlock: ConversationBlock | null
  sessionId: string | null
}) {
  const isContinuation =
    prevBlock !== null && isAssistantLike(block) && isAssistantLike(prevBlock)

  return (
    <div
      className={cn(
        'mx-auto w-[min(100%,var(--layout-content-max-width))] min-w-0 transition-[margin-top] duration-200 ease-out',
        isContinuation && '-mt-[32px]'
      )}
    >
      {block.kind === 'assistant' ? (
        <AssistantMessage
          block={block}
          reasoningText={block.reasoningContent ?? null}
        />
      ) : block.kind === 'user' ? (
        <UserMessage block={block} />
      ) : block.kind === 'toolCall' ? (
        <ToolCallBlock block={block} sessionId={sessionId} />
      ) : block.kind === 'error' ? (
        <ErrorBlock block={block} />
      ) : block.kind === 'systemNote' ? (
        <SystemNote block={block} />
      ) : block.kind === 'compactSummary' ? (
        <CompactSummaryCard block={block} />
      ) : null}
    </div>
  )
})

const BLOCK_GAP_PX = 40 // matches Tailwind gap-10 (2.5rem ≈ 40px)

export default function MessageList({ blocks, sessionId }: MessageListProps) {
  'use no memo'
  // TanStack Virtual returns mutable functions, so this component opts out of React Compiler memoization.
  const listRef = useRef<HTMLDivElement>(null)
  const contentRef = useRef<HTMLDivElement>(null)

  const allItems = useMemo(() => {
    const items: { type: 'block'; block: ConversationBlock; index: number }[] =
      []
    for (let i = 0; i < blocks.length; i++) {
      const block = blocks[i]
      items.push({ type: 'block', block, index: i })
    }
    return items
  }, [blocks])

  const totalItemCount = allItems.length

  // TanStack Virtual returns mutable functions; React Compiler intentionally skips this component.
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: totalItemCount,
    getScrollElement: () => listRef.current,
    estimateSize: (index) => {
      const block = allItems[index].block
      if (block.kind === 'user') return 80
      if (block.kind === 'systemNote') return 60
      if (block.kind === 'error') return 80
      if (block.kind === 'compactSummary') return 120
      if (block.kind === 'toolCall') return 160
      return 120
    },
    overscan: 5,
    getItemKey: (index) => allItems[index].block.id,
  })

  const virtualizerRef = useRef<FollowLatestVirtualizer | null>(null)
  virtualizerRef.current = virtualizer

  // Stable streaming block identifier — changes only when streaming starts/stops
  // or the streaming block changes, NOT on every text delta.  This prevents the
  // ResizeObserver from being torn down and recreated ~30-60 times/sec during
  // fast output, which was the main source of layout thrashing.
  const streamingBlockId = useMemo(() => {
    const last = blocks[blocks.length - 1]
    if (
      last &&
      (last.kind === 'assistant' || last.kind === 'toolCall') &&
      last.status === 'streaming'
    ) {
      return last.id
    }
    return null
  }, [blocks])

  const { handleScroll, handleWheel, handleTouchStart, handleTouchMove } =
    useFollowLatestScroll({
      listRef,
      contentRef,
      virtualizerRef,
      itemCount: totalItemCount,
      sessionId,
      streamingBlockId,
    })

  const virtualItems = virtualizer.getVirtualItems()

  return (
    <div
      ref={listRef}
      className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto bg-panel-bg px-[var(--layout-page-padding-x)] py-7"
      onScroll={handleScroll}
      onWheel={handleWheel}
      onTouchStart={handleTouchStart}
      onTouchMove={handleTouchMove}
    >
      {blocks.length === 0 && (
        <div
          className={cn(
            emptyStateSurface,
            'mx-auto mt-[90px] w-[min(100%,var(--layout-content-max-width))]'
          )}
        >
          {sessionId ? (
            <>
              <p className="mb-1 text-[15px] font-medium text-text-primary">
                向 AstrCode 提问，开始对话
              </p>
              <p className="text-[13px] text-text-muted">
                输入问题，或使用 / 查看可用命令
              </p>
            </>
          ) : (
            '选择或创建一个会话'
          )}
        </div>
      )}

      {blocks.length > 0 && (
        <div
          ref={contentRef}
          style={{
            height: virtualizer.getTotalSize(),
            width: '100%',
            position: 'relative',
          }}
        >
          {virtualItems.map((virtualItem) => {
            const { block, index } = allItems[virtualItem.index]
            const prevBlock = index > 0 ? blocks[index - 1] : null
            const content = (
              <BlockRenderer
                block={block}
                prevBlock={prevBlock}
                sessionId={sessionId}
              />
            )

            // First item has no top gap; subsequent items get gap via padding
            const topPadding = virtualItem.index === 0 ? 0 : BLOCK_GAP_PX

            return (
              <div
                key={virtualItem.key}
                data-index={virtualItem.index}
                ref={virtualizer.measureElement}
                style={{
                  position: 'absolute',
                  top: 0,
                  left: 0,
                  width: '100%',
                  transform: `translateY(${virtualItem.start}px)`,
                  paddingTop: topPadding,
                }}
              >
                {content}
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
