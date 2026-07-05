import { memo } from 'react'
import {
  useElapsedSeconds,
  runningElapsedLabel,
} from '../../hooks/useElapsedSeconds'
import { cn } from '../../lib/utils'
import { toolApprovalPending, type ToolUiContext } from '../../tool-ui'
import { extractRenderSpec } from '../../types/render-spec'
import { readGateApproval } from '../../tool-ui/components/gateApprovalMeta'
import { Icon, type IconName } from '../ui/Icon'
import { AssistantMessageContent } from './AssistantMessage'
import { MarkdownContent, StreamingMarkdown } from './MarkdownContent'
import ToolCallBlock from './ToolCallBlock'
import {
  buildAssistantRunModel,
  type AssistantLikeBlock,
  type AssistantRunSegment,
  type ProcessEntry,
  type ToolActivity,
  processSummaryTitle,
} from './assistantRunModel'
import { toolArgs, toolMeta } from './tools/helpers'

interface AssistantRunMessageProps {
  blocks: AssistantLikeBlock[]
  sessionId: string | null
}

function toolNeedsAttention(
  block: ToolActivity['block'],
  sessionId: string | null
) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const ctx: ToolUiContext = {
    block,
    sessionId,
    args,
    meta,
    renderSpec: extractRenderSpec(block.metadata),
  }
  return (
    block.status === 'error' ||
    readGateApproval(block.metadata)?.pending === true ||
    toolApprovalPending(ctx)
  )
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
        activity.block.status === 'error'
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
  shouldAutoOpen,
}: {
  title: string
  entries: ProcessEntry[]
  sessionId: string | null
  hasFollowingContent: boolean
  shouldAutoOpen: boolean
}) {
  if (entries.length === 0) return null
  const autoOpenProps = shouldAutoOpen ? { open: true } : {}

  return (
    <details
      key={shouldAutoOpen ? 'auto-open' : 'manual-closed'}
      className={cn(
        'group bg-transparent border-none rounded-0 overflow-visible',
        hasFollowingContent ? 'mb-2.5' : 'my-2.5'
      )}
      {...autoOpenProps}
    >
      <summary className="inline-flex max-w-full cursor-pointer list-none items-center gap-2 py-1 text-[15px] font-medium leading-relaxed text-text-muted select-none transition-colors duration-150 hover:text-text-secondary [&::-webkit-details-marker]:hidden">
        <span className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap">
          {title}
        </span>
        <span className="inline-flex h-4 w-4 shrink-0 items-center justify-center text-text-muted/90 transition-transform duration-150 ease-out group-open:rotate-90">
          <Icon name="chevron-right" size={16} />
        </span>
      </summary>

      <div className="mt-1.5 min-w-0 pb-1">
        <div className="space-y-2">
          {entries.map((entry) => {
            if (entry.type === 'thinking') {
              return (
                <div
                  key={entry.id}
                  className="prose-chat border-l-2 border-border pl-4 text-[14.5px] leading-relaxed text-text-primary"
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
    </details>
  )
}

function segmentNeedsAttention(
  segment: AssistantRunSegment,
  sessionId: string | null
) {
  if (segment.type !== 'process') return false
  return (
    segment.hasAttention ||
    segment.entries.some(
      (entry) =>
        entry.type === 'tool' &&
        toolNeedsAttention(entry.activity.block, sessionId)
    )
  )
}

function AssistantRunMessage({ blocks, sessionId }: AssistantRunMessageProps) {
  const runModel = buildAssistantRunModel(blocks)

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
            const shouldAutoOpen =
              segment.hasStreamingWork ||
              segmentNeedsAttention(segment, sessionId)

            return (
              <ProcessSummary
                key={segment.id}
                title={processSummaryTitle(segment)}
                entries={segment.entries}
                sessionId={sessionId}
                hasFollowingContent={nextSegment?.type === 'content'}
                shouldAutoOpen={shouldAutoOpen}
              />
            )
          })}
        </div>
      </div>
    </div>
  )
}

export default memo(AssistantRunMessage)
