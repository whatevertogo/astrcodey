import { useCallback, useEffect, useRef, useMemo } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'
import { emptyStateSurface } from '../../lib/styles'
import { useAppStore } from '../../store/conversation'
import AssistantMessage from './AssistantMessage'
import UserMessage from './UserMessage'
import ToolCallBlock from './ToolCallBlock'
import ErrorBlock from './ErrorBlock'
import SystemNote from './SystemNote'
import CompactSummaryCard from './CompactSummaryCard'

interface MessageListProps {
  blocks: ConversationBlock[]
  sessionId: string | null
}

function isAssistantLike(block: ConversationBlock): boolean {
  return block.kind === 'assistant' || block.kind === 'toolCall'
}

function BlockRenderer({
  block,
  prevBlock,
}: {
  block: ConversationBlock
  prevBlock: ConversationBlock | null
}) {
  const isContinuation =
    prevBlock !== null && isAssistantLike(block) && isAssistantLike(prevBlock)

  return (
    <div
      className={cn(
        'mx-auto w-[min(100%,var(--chat-content-max-width))] min-w-0 transition-[margin-top] duration-200 ease-out',
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
        <ToolCallBlock block={block} />
      ) : block.kind === 'error' ? (
        <ErrorBlock block={block} />
      ) : block.kind === 'systemNote' ? (
        <SystemNote block={block} />
      ) : block.kind === 'compactSummary' ? (
        <CompactSummaryCard block={block} />
      ) : null}
    </div>
  )
}

const BLOCK_GAP_PX = 40 // matches Tailwind gap-10 (2.5rem ≈ 40px)

export default function MessageList({ blocks, sessionId }: MessageListProps) {
  const listRef = useRef<HTMLDivElement>(null)
  const shouldStickRef = useRef(true)
  const prevLengthRef = useRef(0)
  const queuedMessages = useAppStore((s) => s.queuedMessages)
  const phase = useAppStore((s) => s.phase)

  // All items: blocks + queued messages
  const allItems = useMemo(() => {
    const items: { type: 'block'; block: ConversationBlock; index: number }[] =
      []
    for (let i = 0; i < blocks.length; i++) {
      items.push({ type: 'block', block: blocks[i], index: i })
    }
    return items
  }, [blocks])

  const totalItemCount = allItems.length + queuedMessages.length

  const virtualizer = useVirtualizer({
    count: totalItemCount,
    getScrollElement: () => listRef.current,
    estimateSize: (index) => {
      if (index >= allItems.length) return 80 // queued message
      const block = allItems[index].block
      if (block.kind === 'user') return 80
      if (block.kind === 'systemNote') return 60
      if (block.kind === 'error') return 80
      if (block.kind === 'compactSummary') return 120
      if (block.kind === 'toolCall') return 160
      return 120
    },
    overscan: 5,
    getItemKey: (index) => {
      if (index < allItems.length) {
        return allItems[index].block.id
      }
      return `queued-${index - allItems.length}`
    },
  })

  const updateStickiness = useCallback(() => {
    const container = listRef.current
    if (!container) {
      shouldStickRef.current = true
      return
    }
    const distanceFromBottom =
      container.scrollHeight - container.scrollTop - container.clientHeight
    shouldStickRef.current = distanceFromBottom <= 48
  }, [])

  // User scroll-up gesture immediately breaks auto-stick
  const handleWheel = useCallback((e: React.WheelEvent<HTMLDivElement>) => {
    if (e.deltaY < 0) {
      shouldStickRef.current = false
    }
  }, [])

  // Auto-scroll: stick to bottom when appropriate
  useEffect(() => {
    const shouldAutoScroll =
      prevLengthRef.current === 0 || shouldStickRef.current
    prevLengthRef.current = blocks.length + queuedMessages.length
    if (!shouldAutoScroll) return

    requestAnimationFrame(() => {
      if (totalItemCount > 0) {
        virtualizer.scrollToIndex(totalItemCount - 1, { align: 'end' })
      }
      updateStickiness()
    })
  }, [
    blocks,
    queuedMessages.length,
    updateStickiness,
    totalItemCount,
    virtualizer,
  ])

  // During active streaming, continuously stick to bottom
  const isStreaming =
    phase === 'streaming' || phase === 'thinking' || phase === 'calling_tool'
  useEffect(() => {
    if (!isStreaming || !shouldStickRef.current) return
    const interval = setInterval(() => {
      if (totalItemCount > 0 && shouldStickRef.current) {
        virtualizer.scrollToIndex(totalItemCount - 1, { align: 'end' })
      }
    }, 100)
    return () => clearInterval(interval)
  }, [isStreaming, totalItemCount, virtualizer])

  const virtualItems = virtualizer.getVirtualItems()

  return (
    <div
      ref={listRef}
      className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto bg-panel-bg px-[var(--chat-content-horizontal-padding)] py-7"
      onScroll={updateStickiness}
      onWheel={handleWheel}
    >
      {blocks.length === 0 && (
        <div
          className={cn(
            emptyStateSurface,
            'mx-auto mt-[90px] w-[min(100%,var(--chat-content-max-width))]'
          )}
        >
          {sessionId ? '向 AstrCode 提问，开始对话...' : '选择或创建一个会话'}
        </div>
      )}

      {blocks.length > 0 && (
        <div
          style={{
            height: virtualizer.getTotalSize(),
            width: '100%',
            position: 'relative',
          }}
        >
          {virtualItems.map((virtualItem) => {
            const queueStartIndex = allItems.length
            let content: React.ReactNode

            if (virtualItem.index < queueStartIndex) {
              const { block, index } = allItems[virtualItem.index]
              const prevBlock = index > 0 ? blocks[index - 1] : null
              content = <BlockRenderer block={block} prevBlock={prevBlock} />
            } else {
              const qi = virtualItem.index - queueStartIndex
              const text = queuedMessages[qi]
              content =
                text !== undefined ? (
                  <div className="mx-auto w-[min(100%,var(--chat-content-max-width))] min-w-0">
                    <div className="flex justify-end">
                      <div className="max-w-[80%] rounded-2xl rounded-br-md border border-dashed border-border bg-user-bubble/60 px-4 py-3 text-[15px] leading-[1.7] text-text-primary">
                        <span className="mr-2 text-[11px] text-text-secondary">
                          排队中
                        </span>
                        {text}
                      </div>
                    </div>
                  </div>
                ) : null
            }

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
