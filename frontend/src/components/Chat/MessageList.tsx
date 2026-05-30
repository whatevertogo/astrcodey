import { memo, useCallback, useEffect, useRef, useMemo } from 'react'
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

const BlockRenderer = memo(function BlockRenderer({
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
})

const BLOCK_GAP_PX = 40 // matches Tailwind gap-10 (2.5rem ≈ 40px)
/** Within this distance from the bottom we treat the user as "following" the stream. */
const STICK_TO_BOTTOM_THRESHOLD_PX = 64

function isNearBottom(
  container: HTMLDivElement,
  threshold = STICK_TO_BOTTOM_THRESHOLD_PX
) {
  return (
    container.scrollHeight - container.scrollTop - container.clientHeight <=
    threshold
  )
}

export default function MessageList({ blocks, sessionId }: MessageListProps) {
  const listRef = useRef<HTMLDivElement>(null)
  const contentRef = useRef<HTMLDivElement>(null)
  const shouldStickRef = useRef(true)
  const prevItemCountRef = useRef(0)
  const lastScrollTopRef = useRef(0)
  const ignoreScrollRef = useRef(false)
  const touchStartYRef = useRef<number | null>(null)
  const queuedMessages = useAppStore((s) => s.queuedMessages)

  const scrollContainerToBottom = useCallback(
    (behavior: ScrollBehavior = 'auto') => {
      const container = listRef.current
      if (!container) return
      ignoreScrollRef.current = true
      container.scrollTo({ top: container.scrollHeight, behavior })
      requestAnimationFrame(() => {
        lastScrollTopRef.current = container.scrollTop
        ignoreScrollRef.current = false
      })
    },
    []
  )

  const allItems = useMemo(() => {
    const items: { type: 'block'; block: ConversationBlock; index: number }[] =
      []
    for (let i = 0; i < blocks.length; i++) {
      const block = blocks[i]
      items.push({ type: 'block', block, index: i })
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

  const virtualizerRef = useRef(virtualizer)
  virtualizerRef.current = virtualizer

  const followLatest = useCallback(
    (behavior: ScrollBehavior = 'auto') => {
      if (!shouldStickRef.current) return
      const itemCount = prevItemCountRef.current
      if (itemCount > 0) {
        virtualizerRef.current.scrollToIndex(itemCount - 1, { align: 'end' })
      }
      scrollContainerToBottom(behavior)
    },
    [scrollContainerToBottom]
  )

  const markUserScrolledUp = useCallback(() => {
    shouldStickRef.current = false
  }, [])

  const updateStickiness = useCallback(() => {
    if (ignoreScrollRef.current) return

    const container = listRef.current
    if (!container) return

    const scrollTop = container.scrollTop

    // Only react to user-driven scroll direction — do not disable follow when
    // content grows below the viewport (scrollTop unchanged, distance increases).
    if (scrollTop < lastScrollTopRef.current - 2) {
      shouldStickRef.current = false
    } else if (
      scrollTop > lastScrollTopRef.current + 2 &&
      isNearBottom(container)
    ) {
      shouldStickRef.current = true
    }

    lastScrollTopRef.current = scrollTop
  }, [])

  const handleWheel = useCallback(
    (e: React.WheelEvent<HTMLDivElement>) => {
      if (e.deltaY < 0) {
        markUserScrolledUp()
      }
    },
    [markUserScrolledUp]
  )

  const handleTouchStart = useCallback(
    (e: React.TouchEvent<HTMLDivElement>) => {
      touchStartYRef.current = e.touches[0]?.clientY ?? null
    },
    []
  )

  const handleTouchMove = useCallback(
    (e: React.TouchEvent<HTMLDivElement>) => {
      const startY = touchStartYRef.current
      const currentY = e.touches[0]?.clientY
      if (startY === null || currentY === undefined) return
      if (currentY > startY + 4) {
        markUserScrolledUp()
      }
    },
    [markUserScrolledUp]
  )

  // New session: default to following the latest messages.
  useEffect(() => {
    shouldStickRef.current = true
    prevItemCountRef.current = 0
    lastScrollTopRef.current = 0
  }, [sessionId])

  const streamingTailSignature = useMemo(() => {
    const last = blocks[blocks.length - 1]
    if (last?.kind === 'assistant' && last.status === 'streaming') {
      const textLen = last.text?.length ?? 0
      const reasoningLen = last.reasoningContent?.length ?? 0
      return `${last.id}:${textLen}:${reasoningLen}`
    }
    return null
  }, [blocks])

  // New block / queued message: scroll only when the list grows and user is following.
  useEffect(() => {
    const itemCount = totalItemCount
    const isFirstPaint = prevItemCountRef.current === 0 && itemCount > 0
    const grew = itemCount > prevItemCountRef.current
    prevItemCountRef.current = itemCount

    if (!grew && !isFirstPaint) return
    if (!shouldStickRef.current && !isFirstPaint) return

    const frame = requestAnimationFrame(() => {
      if (!shouldStickRef.current && !isFirstPaint) return
      if (itemCount === 0) return
      followLatest()
    })
    return () => cancelAnimationFrame(frame)
  }, [totalItemCount, followLatest])

  // Streaming text growth: keep pinned when the user is following.
  useEffect(() => {
    if (!streamingTailSignature) return

    const frame = requestAnimationFrame(() => {
      if (!shouldStickRef.current) return
      followLatest()
    })
    return () => cancelAnimationFrame(frame)
  }, [streamingTailSignature, followLatest])

  // Virtual list remeasures asynchronously; follow again after layout settles.
  useEffect(() => {
    if (!streamingTailSignature) return
    const content = contentRef.current
    if (!content) return

    let raf = 0
    const observer = new ResizeObserver(() => {
      if (!shouldStickRef.current) return
      cancelAnimationFrame(raf)
      raf = requestAnimationFrame(() => {
        if (!shouldStickRef.current) return
        followLatest()
      })
    })
    observer.observe(content)
    return () => {
      cancelAnimationFrame(raf)
      observer.disconnect()
    }
  }, [streamingTailSignature, followLatest])

  const virtualItems = virtualizer.getVirtualItems()

  return (
    <div
      ref={listRef}
      className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto bg-panel-bg px-[var(--chat-content-horizontal-padding)] py-7"
      onScroll={updateStickiness}
      onWheel={handleWheel}
      onTouchStart={handleTouchStart}
      onTouchMove={handleTouchMove}
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
          ref={contentRef}
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
