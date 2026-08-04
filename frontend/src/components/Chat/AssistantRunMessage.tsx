import { memo, useCallback, useState } from 'react'
import {
  useElapsedSeconds,
  runningElapsedLabel,
} from '../../hooks/useElapsedSeconds'
import { ghostIconButton } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { toolCallHasError, toolCallIsTerminal } from '../../services/types'
import { useAppStore } from '../../store/conversation'
import { Icon, type IconName } from '../ui/Icon'
import { AssistantMessageContent } from './AssistantMessage'
import { MarkdownContent, StreamingMarkdown } from './MarkdownContent'
import ToolCallBlock from './ToolCallBlock'
import { AskUserCard } from './tools/AskUserCard'
import { toolArgs } from './tools/helpers'
import {
  assistantRunCompletedReply,
  assistantRunCopyText,
  buildAssistantRunModel,
  type AssistantLikeBlock,
  type AssistantRunSegment,
  type ProcessEntry,
  type ToolActivity,
  processSummaryTitle,
} from './assistantRunModel'
import { isPendingAskUser } from './tools/askUser'

interface AssistantRunMessageProps {
  blocks: AssistantLikeBlock[]
  sessionId: string | null
}

function AssistantRunActions({
  copyText,
  sessionId,
  storageSeq,
}: {
  copyText: string
  sessionId: string | null
  storageSeq?: number
}) {
  const forkSession = useAppStore((state) => state.forkSession)
  const [copied, setCopied] = useState(false)
  const handleCopy = useCallback(() => {
    void navigator.clipboard
      .writeText(copyText)
      .then(() => {
        setCopied(true)
        window.setTimeout(() => setCopied(false), 2000)
      })
      .catch(() => undefined)
  }, [copyText])

  return (
    <div
      className="flex min-h-8 items-center gap-1 pt-1 text-text-muted opacity-60 transition-opacity duration-150 hover:opacity-100 focus-within:opacity-100 motion-reduce:transition-none"
      role="group"
      aria-label="Turn 操作"
    >
      <button
        type="button"
        className={cn(
          ghostIconButton,
          'h-7 gap-1.5 rounded-md px-2 text-[12px] font-medium'
        )}
        onClick={handleCopy}
        aria-label={copied ? '已复制' : '复制此 Turn'}
      >
        <Icon name="copy" size={15} />
        {copied ? '已复制' : '复制'}
      </button>
      {sessionId && storageSeq != null ? (
        <button
          type="button"
          className={cn(
            ghostIconButton,
            'h-7 gap-1.5 rounded-md px-2 text-[12px] font-medium'
          )}
          onClick={() => void forkSession(sessionId, storageSeq)}
          aria-label="从此 Turn 分叉"
        >
          <Icon name="branch" size={15} />
          分叉
        </button>
      ) : null}
      <span className="sr-only" aria-live="polite">
        {copied ? '已复制' : ''}
      </span>
    </div>
  )
}

function toolNeedsAttention(block: ToolActivity['block']) {
  if (toolCallIsTerminal(block.status)) return false
  return block.approval != null || isPendingAskUser(block)
}

function activityIconName(activity: ToolActivity): IconName {
  if (activity.kind === 'command') return 'monitor'
  if (activity.kind === 'tool') return 'plug'
  return 'edit'
}

function ActivitySummaryContent({ activity }: { activity: ToolActivity }) {
  const streaming = activity.block.status === 'streaming'
  const elapsed = useElapsedSeconds(streaming)
  const commandRuntime =
    activity.kind === 'command' && streaming
      ? runningElapsedLabel(elapsed, 'zh').replace('运行中', '已持续')
      : activity.detail

  return (
    <span
      className={cn(
        'flex min-w-0 flex-wrap items-baseline gap-x-1.5 gap-y-1 text-[14px] leading-snug',
        toolCallHasError(activity.block.status)
          ? 'text-danger'
          : 'text-text-secondary'
      )}
    >
      <span className="min-w-0 overflow-wrap-anywhere font-medium text-accent">
        {activity.label}
      </span>
      {activity.insertions != null ? (
        <span className="shrink-0 text-success">+{activity.insertions}</span>
      ) : null}
      {activity.deletions != null ? (
        <span className="shrink-0 text-danger">-{activity.deletions}</span>
      ) : null}
      {commandRuntime ? (
        <span className="shrink-0 text-text-muted">，{commandRuntime}</span>
      ) : null}
      {streaming ? (
        <span className="mt-0.5 h-2 w-2 shrink-0 rounded-full bg-accent/60" />
      ) : null}
    </span>
  )
}

function ActivityToolRow({
  activity,
  sessionId,
}: {
  activity: ToolActivity
  sessionId: string | null
}) {
  return (
    <ToolCallBlock
      block={activity.block}
      sessionId={sessionId}
      embedded
      summaryIconName={activityIconName(activity)}
      summaryContent={<ActivitySummaryContent activity={activity} />}
    />
  )
}

function ProcessSummary({
  title,
  entries,
  sessionId,
  hasFollowingContent,
  forceOpen,
}: {
  title: string
  entries: ProcessEntry[]
  sessionId: string | null
  hasFollowingContent: boolean
  forceOpen: boolean
}) {
  const [userOpen, setUserOpen] = useState(false)
  if (entries.length === 0) return null
  const open = forceOpen || userOpen
  const hasError = entries.some(
    (entry) =>
      entry.type === 'tool' && toolCallHasError(entry.activity.block.status)
  )
  const latestEntry = entries[entries.length - 1]
  const latestLabel =
    latestEntry.type === 'tool'
      ? `${latestEntry.activity.title} ${latestEntry.activity.label}`
      : latestEntry.entry.streaming
        ? '正在思考'
        : '思考过程'

  return (
    <details
      className={cn(
        'group bg-transparent border-none rounded-0 overflow-visible',
        hasFollowingContent ? 'mb-2.5' : 'my-2.5'
      )}
      open={open}
      onToggle={(event) => {
        if (forceOpen) {
          if (!event.currentTarget.open) {
            event.currentTarget.open = true
          }
          return
        }
        setUserOpen(event.currentTarget.open)
      }}
    >
      <summary className="inline-flex max-w-full cursor-pointer list-none items-center gap-2 rounded-md py-1 text-[14px] font-medium leading-relaxed text-text-muted select-none transition-colors duration-150 hover:text-text-secondary [&::-webkit-details-marker]:hidden">
        <span className={cn('shrink-0', hasError && 'text-danger')}>
          {hasError ? '处理失败' : title}
        </span>
        <span className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[13px] font-normal text-text-muted/80">
          {latestLabel}
        </span>
        {entries.length > 1 ? (
          <span className="shrink-0 text-[11px] font-normal text-text-muted/70">
            {entries.length} 项
          </span>
        ) : null}
        <span className="inline-flex h-4 w-4 shrink-0 items-center justify-center text-text-muted/90 transition-transform duration-150 ease-out group-open:rotate-90">
          <Icon name="chevron-right" size={16} />
        </span>
      </summary>

      {open ? (
        <div className="mt-1.5 min-w-0 border-l border-border pb-1 pl-3">
          <div className="space-y-1.5">
            {entries.map((entry) => {
              if (entry.type === 'thinking') {
                return (
                  <div
                    key={entry.id}
                    className="prose-chat py-1 text-[13.5px] leading-relaxed text-text-secondary"
                  >
                    {entry.entry.streaming ? (
                      <StreamingMarkdown
                        text={entry.entry.text}
                        cacheKey={`${entry.entry.blockId}:run-thinking`}
                      />
                    ) : (
                      <MarkdownContent text={entry.entry.text} />
                    )}
                  </div>
                )
              }

              return (
                <ActivityToolRow
                  key={entry.id}
                  activity={entry.activity}
                  sessionId={sessionId}
                />
              )
            })}
          </div>
        </div>
      ) : null}
    </details>
  )
}

function segmentNeedsAttention(segment: AssistantRunSegment) {
  if (segment.type !== 'process') return false
  return (
    segment.hasAttention ||
    segment.entries.some(
      (entry) =>
        entry.type === 'tool' && toolNeedsAttention(entry.activity.block)
    )
  )
}

function isPendingAskUserEntry(entry: ProcessEntry): boolean {
  return entry.type === 'tool' && isPendingAskUser(entry.activity.block)
}

/// 待回答的 askUser 问题：从 process 折叠中提取出来，直接渲染在消息流里。
/// 完成后（block 不再 streaming）自然回到 process 折叠中显示结果。
function PendingAskUserPrompts({
  entries,
  sessionId,
}: {
  entries: Extract<ProcessEntry, { type: 'tool' }>[]
  sessionId: string | null
}) {
  const askUserEntries = entries.filter(isPendingAskUserEntry)
  if (askUserEntries.length === 0) return null

  return (
    <div className="space-y-2">
      {askUserEntries.map((entry) => (
        <div
          key={entry.id}
          className="rounded-lg border border-accent/30 bg-accent/5 p-3"
        >
          <AskUserCard
            block={entry.activity.block}
            sessionId={sessionId}
            args={toolArgs(entry.activity.block)}
          />
        </div>
      ))}
    </div>
  )
}

function AssistantRunMessage({ blocks, sessionId }: AssistantRunMessageProps) {
  const runModel = buildAssistantRunModel(blocks)
  const completedReply = assistantRunCompletedReply(blocks)
  const copyText = completedReply ? assistantRunCopyText(blocks) : ''

  return (
    <div className="flex items-start animate-message-enter motion-reduce:animate-none">
      <div className="min-w-0 flex-1 pt-0.5">
        <div className="relative min-w-0 max-w-full overflow-wrap-anywhere bg-transparent py-2 text-text-primary prose-chat">
          {runModel.segments.map((segment, index) => {
            if (segment.type === 'content') {
              return (
                <AssistantMessageContent
                  key={segment.id}
                  block={segment.block}
                  reasoningText={segment.block.reasoningContent ?? null}
                  showThinking={false}
                />
              )
            }

            const nextSegment = runModel.segments[index + 1]
            const forceOpen = segmentNeedsAttention(segment)
            const pendingAskUser = segment.entries.filter(
              isPendingAskUserEntry
            ) as Extract<ProcessEntry, { type: 'tool' }>[]
            const remainingEntries = segment.entries.filter(
              (entry) => !isPendingAskUserEntry(entry)
            )

            return (
              <div key={segment.id} className="space-y-2">
                <PendingAskUserPrompts
                  entries={pendingAskUser}
                  sessionId={sessionId}
                />
                {remainingEntries.length > 0 ? (
                  <ProcessSummary
                    title={processSummaryTitle(segment)}
                    entries={remainingEntries}
                    sessionId={sessionId}
                    hasFollowingContent={nextSegment?.type === 'content'}
                    forceOpen={forceOpen}
                  />
                ) : null}
              </div>
            )
          })}
        </div>
        {completedReply && copyText ? (
          <AssistantRunActions
            copyText={copyText}
            sessionId={sessionId}
            storageSeq={completedReply.storageSeq}
          />
        ) : null}
      </div>
    </div>
  )
}

export default memo(AssistantRunMessage)
