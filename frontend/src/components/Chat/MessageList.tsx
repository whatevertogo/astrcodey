import { memo, useRef, useMemo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'
import { emptyStateSurface } from '../../lib/styles'
import AssistantRunMessage from './AssistantRunMessage'
import UserMessage from './UserMessage'
import ErrorBlock from './ErrorBlock'
import SystemNote from './SystemNote'
import CompactSummaryCard from './CompactSummaryCard'
import { useFollowLatestScroll } from './useFollowLatestScroll'
import {
  buildMessageListItems,
  streamingMessageListItemId,
  type MessageListItem,
} from './assistantRunModel'

interface MessageListProps {
  blocks: ConversationBlock[]
  sessionId: string | null
}

const BlockRenderer = memo(function BlockRenderer({
  item,
  sessionId,
}: {
  item: MessageListItem
  sessionId: string | null
}) {
  return (
    <div className="mx-auto w-[min(100%,var(--layout-content-max-width))] min-w-0">
      {item.type === 'assistantRun' ? (
        <AssistantRunMessage blocks={item.blocks} sessionId={sessionId} />
      ) : item.block.kind === 'user' ? (
        <UserMessage block={item.block} />
      ) : item.block.kind === 'error' ? (
        <ErrorBlock block={item.block} />
      ) : item.block.kind === 'systemNote' ? (
        <SystemNote block={item.block} />
      ) : item.block.kind === 'compactSummary' ? (
        <CompactSummaryCard block={item.block} />
      ) : null}
    </div>
  )
})

const BLOCK_GAP_PX = 22

export default function MessageList({ blocks, sessionId }: MessageListProps) {
  const listRef = useRef<HTMLDivElement>(null)
  const contentRef = useRef<HTMLDivElement>(null)

  const allItems = useMemo(() => buildMessageListItems(blocks), [blocks])

  const totalItemCount = allItems.length

  // Stable streaming block identifier — changes only when streaming starts/stops
  // or the streaming block changes, NOT on every text delta.  This prevents the
  // ResizeObserver from being torn down and recreated ~30-60 times/sec during
  // fast output, which was the main source of layout thrashing.
  const streamingBlockId = useMemo(
    () => streamingMessageListItemId(allItems),
    [allItems]
  )

  const { handleScroll, handleWheel, handleTouchStart, handleTouchMove } =
    useFollowLatestScroll({
      listRef,
      contentRef,
      itemCount: totalItemCount,
      sessionId,
      streamingBlockId,
    })

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
          className="flex w-full flex-col"
          style={{
            gap: BLOCK_GAP_PX,
            overflowAnchor: 'none',
          }}
        >
          {allItems.map((item) => (
            <BlockRenderer key={item.id} item={item} sessionId={sessionId} />
          ))}
        </div>
      )}
    </div>
  )
}
