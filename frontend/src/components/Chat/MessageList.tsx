import { useCallback, useEffect, useRef } from 'react'
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

export default function MessageList({ blocks, sessionId }: MessageListProps) {
  const listRef = useRef<HTMLDivElement>(null)
  const bottomRef = useRef<HTMLDivElement>(null)
  const shouldStickRef = useRef(true)
  const prevLengthRef = useRef(0)
  const queuedMessages = useAppStore((s) => s.queuedMessages)

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

  useEffect(() => {
    const shouldAutoScroll =
      prevLengthRef.current === 0 || shouldStickRef.current
    prevLengthRef.current = blocks.length + queuedMessages.length
    if (!shouldAutoScroll) return

    requestAnimationFrame(() => {
      if (listRef.current) {
        listRef.current.scrollTop = listRef.current.scrollHeight
      }
      updateStickiness()
    })
  }, [blocks, queuedMessages.length, updateStickiness])

  const renderedBlocks = blocks.map((block, index) => {
    const prevBlock = index > 0 ? blocks[index - 1] : null
    const isContinuation =
      prevBlock !== null && isAssistantLike(block) && isAssistantLike(prevBlock)

    return (
      <div
        key={block.id}
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

  return (
    <div
      ref={listRef}
      className="flex min-w-0 flex-1 flex-col gap-10 overflow-x-hidden overflow-y-auto bg-panel-bg px-[var(--chat-content-horizontal-padding)] py-7"
      onScroll={updateStickiness}
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
      {renderedBlocks}
      {queuedMessages.map((text, index) => (
        <div
          key={`queued-${index}`}
          className="mx-auto w-[min(100%,var(--chat-content-max-width))] min-w-0"
        >
          <div className="flex justify-end">
            <div className="max-w-[80%] rounded-2xl rounded-br-md border border-dashed border-border bg-user-bubble/60 px-4 py-3 text-[15px] leading-[1.7] text-text-primary">
              <span className="mr-2 text-[11px] text-text-secondary">
                排队中
              </span>
              {text}
            </div>
          </div>
        </div>
      ))}
      <div ref={bottomRef} />
    </div>
  )
}
