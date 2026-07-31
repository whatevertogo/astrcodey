import {
  memo,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { useVirtualizer, type Virtualizer } from '@tanstack/react-virtual'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'
import { emptyStateSurface } from '../../lib/styles'
import { useAppStore } from '../../store/conversation'
import AssistantRunMessage from './AssistantRunMessage'
import UserMessage from './UserMessage'
import ErrorBlock from './ErrorBlock'
import SystemNote from './SystemNote'
import CompactSummaryCard from './CompactSummaryCard'
import RecapBlock from './RecapBlock'
import {
  buildMessageListItems,
  type MessageListItem,
} from './assistantRunModel'
import { Icon } from '../ui/Icon'

interface MessageListProps {
  blocks: ConversationBlock[]
  sessionId: string | null
}

function sameRenderedItem(left: MessageListItem, right: MessageListItem) {
  if (left.type !== right.type || left.id !== right.id) return false
  if (left.type === 'forkRow') return true
  if (left.type === 'block' && right.type === 'block') {
    return left.block === right.block
  }
  if (left.type !== 'assistantRun' || right.type !== 'assistantRun') {
    return false
  }
  return (
    left.blocks.length === right.blocks.length &&
    left.blocks.every((block, index) => block === right.blocks[index])
  )
}

function ForkRow({ sessionId }: { sessionId: string | null }) {
  const forkSession = useAppStore((s) => s.forkSession)
  if (!sessionId) return null
  return (
    <div className="flex items-center gap-3 py-1" aria-hidden={false}>
      <div className="h-px flex-1 border-t border-dashed border-border" />
      <button
        type="button"
        className="inline-flex items-center gap-1.5 rounded-full border border-border bg-surface/60 px-3.5 py-1.5 text-[12px] font-medium text-text-muted transition-colors duration-150 hover:border-accent-strong/40 hover:text-text-primary"
        onClick={() => void forkSession(sessionId)}
        title="从当前会话末尾分叉出一个新会话"
      >
        <Icon name="branch" size={13} />
        从此处分叉
      </button>
      <div className="h-px flex-1 border-t border-dashed border-border" />
    </div>
  )
}

const BlockRenderer = memo(
  function BlockRenderer({
    item,
    sessionId,
  }: {
    item: MessageListItem
    sessionId: string | null
  }) {
    return (
      <div className="mx-auto w-[min(100%,var(--layout-content-max-width))] min-w-0 px-[var(--layout-content-inset-x)]">
        {item.type === 'assistantRun' ? (
          <AssistantRunMessage blocks={item.blocks} sessionId={sessionId} />
        ) : item.type === 'forkRow' ? (
          <ForkRow sessionId={sessionId} />
        ) : item.block.kind === 'user' ? (
          <UserMessage block={item.block} />
        ) : item.block.kind === 'error' ? (
          <ErrorBlock block={item.block} />
        ) : item.block.kind === 'systemNote' ? (
          <SystemNote block={item.block} />
        ) : item.block.kind === 'compactSummary' ? (
          <CompactSummaryCard block={item.block} />
        ) : item.block.kind === 'recap' ? (
          <RecapBlock block={item.block} />
        ) : null}
      </div>
    )
  },
  (previous, next) =>
    previous.sessionId === next.sessionId &&
    sameRenderedItem(previous.item, next.item)
)

const BLOCK_GAP_PX = 22
const SCROLL_END_THRESHOLD_PX = 96

export default function MessageList({ blocks, sessionId }: MessageListProps) {
  const listRef = useRef<HTMLDivElement>(null)
  const positionedSessionRef = useRef<string | null>(null)
  const [showJumpToLatest, setShowJumpToLatest] = useState(false)
  const allItems = useMemo(() => buildMessageListItems(blocks), [blocks])
  const getItemKey = useCallback(
    (index: number) => allItems[index]?.id ?? index,
    [allItems]
  )
  const handleVirtualizerChange = useCallback(
    (virtualizer: Virtualizer<HTMLDivElement, Element>) => {
      const visible = !virtualizer.isAtEnd()
      setShowJumpToLatest((current) =>
        current === visible ? current : visible
      )
    },
    []
  )

  // TanStack Virtual owns a mutable measurement instance by design.
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: allItems.length,
    getScrollElement: () => listRef.current,
    estimateSize: () => 280,
    getItemKey,
    gap: BLOCK_GAP_PX,
    overscan: 6,
    paddingStart: 28,
    paddingEnd: 32,
    anchorTo: 'end',
    followOnAppend: true,
    scrollEndThreshold: SCROLL_END_THRESHOLD_PX,
    onChange: handleVirtualizerChange,
  })

  useLayoutEffect(() => {
    if (!sessionId) {
      positionedSessionRef.current = null
      return
    }
    if (allItems.length === 0 || positionedSessionRef.current === sessionId) {
      return
    }

    const frame = requestAnimationFrame(() => {
      virtualizer.scrollToEnd({ behavior: 'instant' })
      positionedSessionRef.current = sessionId
    })
    return () => cancelAnimationFrame(frame)
  }, [allItems.length, sessionId, virtualizer])

  const virtualItems = virtualizer.getVirtualItems()

  return (
    <div className="relative flex min-h-0 min-w-0 flex-1">
      <div
        ref={listRef}
        data-testid="conversation-scroll"
        className="h-full w-full overflow-x-hidden overflow-y-auto overscroll-contain bg-panel-bg px-[var(--layout-page-padding-x)]"
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
            className="relative w-full"
            style={{
              height: virtualizer.getTotalSize(),
              overflowAnchor: 'none',
            }}
          >
            {virtualItems.map((virtualItem) => {
              const item = allItems[virtualItem.index]
              if (!item) return null
              return (
                <div
                  key={virtualItem.key}
                  ref={virtualizer.measureElement}
                  data-index={virtualItem.index}
                  data-message-item-id={item.id}
                  className="absolute left-0 top-0 w-full"
                  style={{
                    transform: `translateY(${virtualItem.start}px)`,
                  }}
                >
                  <BlockRenderer item={item} sessionId={sessionId} />
                </div>
              )
            })}
          </div>
        )}
      </div>

      {showJumpToLatest && blocks.length > 0 && (
        <button
          type="button"
          className="absolute bottom-4 left-1/2 z-20 inline-flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-border bg-surface/95 px-3 py-1.5 text-[12px] font-medium text-text-secondary shadow-surface-lg backdrop-blur transition-colors hover:bg-surface-muted hover:text-text-primary"
          onClick={() => virtualizer.scrollToEnd({ behavior: 'smooth' })}
          aria-label="跳到最新消息"
        >
          <Icon name="chevron-down" size={14} />
          回到最新
        </button>
      )}
    </div>
  )
}
