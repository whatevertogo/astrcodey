import { memo, useState, type ReactNode } from 'react'
import {
  useElapsedSeconds,
  runningElapsedLabel,
} from '../../hooks/useElapsedSeconds'
import {
  toolCallHasError,
  type AgentSessionLink,
  type ConversationBlock,
} from '../../services/types'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { toolPanelScrollViewport } from '../../lib/styles'
import { RenderSpecViewer } from './RenderSpecViewer'
import './tools/builtinRenderers'
import {
  getToolRenderer,
  type ToolRenderer,
  type ToolRendererContext,
} from './tools/registry'
import {
  compactPreviewLine,
  numberValue,
  statusLabel,
  toolArgs,
  toolMeta,
} from './tools/helpers'
import { DefaultToolDetails } from './tools/shared'
import { buildStreamingAgentSpec } from './tools/agentSpec'
import { AgentChildSessionPanel } from './tools/AgentChildSessionPanel'
import { AskUserCard } from './tools/AskUserCard'
import { askUserSummary, isPendingAskUser } from './tools/askUser'
import { ToolApprovalCard } from './tools/ToolApprovalCard'
import { Icon, type IconName } from '../ui/Icon'

interface ToolCallBlockProps {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
  sessionId: string | null
  embedded?: boolean
  defaultOpen?: boolean
  summaryContent?: ReactNode
  summaryIconName?: IconName
}

function ToolDetails({
  toolContext,
  approvalUi,
  renderer,
  agentChildUi,
}: {
  toolContext: ToolRendererContext
  approvalUi: ReactNode | null
  renderer?: ToolRenderer
  agentChildUi?: ReactNode | null
}) {
  if (toolContext.agentSpec) {
    return (
      <div className="space-y-3">
        <RenderSpecViewer spec={toolContext.agentSpec} />
        {agentChildUi}
      </div>
    )
  }
  if (approvalUi) return approvalUi
  const rendered = renderer?.render?.(toolContext)
  if (rendered != null) {
    return (
      <div className="space-y-3">
        {rendered}
        {agentChildUi}
      </div>
    )
  }
  if (agentChildUi) return agentChildUi
  return <DefaultToolDetails block={toolContext.block} />
}

function DetailRow({
  label,
  children,
}: {
  label: string
  children: ReactNode
}) {
  return (
    <div className="flex min-w-0 flex-col gap-1.5">
      <span className="text-[11px] font-semibold uppercase tracking-wide text-text-muted">
        {label}
      </span>
      {children}
    </div>
  )
}

function DetailValue({ children }: { children: ReactNode }) {
  return (
    <div className="min-w-0 overflow-wrap-anywhere rounded bg-transparent font-mono text-[12px] leading-relaxed text-text-secondary">
      {children}
    </div>
  )
}

function formatArgs(args: Record<string, unknown>, fallback: string): string {
  if (Object.keys(args).length > 0) {
    return JSON.stringify(args, null, 2)
  }
  const trimmed = fallback.trim()
  if (!trimmed) return '{}'
  try {
    return JSON.stringify(JSON.parse(trimmed), null, 2)
  } catch {
    return trimmed
  }
}

function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return ''
  if (seconds < 1) return `${Math.round(seconds * 1000)}ms`
  if (seconds < 60) return `${seconds.toFixed(1)}s`
  const minutes = Math.floor(seconds / 60)
  const restSeconds = Math.round(seconds % 60)
  return `${minutes}m ${restSeconds}s`
}

function durationFromMetadata(meta: Record<string, unknown>): string {
  const durationMs = numberValue(meta, 'durationMs', 'duration_ms')
  if (durationMs != null) return formatDuration(durationMs / 1000)
  const durationSeconds = numberValue(meta, 'duration', 'durationSeconds')
  return durationSeconds != null ? formatDuration(durationSeconds) : ''
}

function ToolCallDetailsPanel({
  context,
  sessionId,
  approvalUi,
  renderer,
  linkedAgent,
  onOpenChild,
  summaryLine,
  gatePending,
}: {
  context: ToolRendererContext
  sessionId: string | null
  approvalUi: ReactNode | null
  renderer?: ToolRenderer
  linkedAgent?: AgentSessionLink
  onOpenChild: (childSessionId: string) => void
  summaryLine: string
  gatePending: boolean
}) {
  const { block, args } = context
  const detailArgs = formatArgs(args, block.arguments)
  const agentChildUi =
    linkedAgent && block.status === 'streaming' ? (
      <AgentChildSessionPanel agent={linkedAgent} onOpenChild={onOpenChild} />
    ) : null

  return (
    <div
      className={cn(
        'min-w-0 overflow-hidden',
        'border-t border-border/70 bg-surface/45 px-3 py-2'
      )}
    >
      <div className={toolPanelScrollViewport}>
        <div className="space-y-3 pb-1">
          <DetailRow label="ID">
            <DetailValue>{block.id}</DetailValue>
          </DetailRow>
          <DetailRow label="Args">
            <pre className="m-0 max-h-[200px] overflow-auto whitespace-pre-wrap bg-transparent font-mono text-[12px] leading-relaxed text-text-secondary">
              {detailArgs}
            </pre>
          </DetailRow>
          {summaryLine && summaryLine !== detailArgs ? (
            <DetailRow label="Summary">
              <DetailValue>{summaryLine}</DetailValue>
            </DetailRow>
          ) : null}
          {gatePending && sessionId && block.approval ? (
            <ToolApprovalCard
              sessionId={sessionId}
              toolName={block.name}
              approval={block.approval}
              args={args}
            />
          ) : (
            <DetailRow label="Result">
              <div
                className={cn(
                  'min-w-0 rounded-lg border border-border bg-surface-soft px-3 py-2',
                  toolCallHasError(block.status) &&
                    'border-danger/25 bg-danger-soft/40',
                  block.status === 'cancelled' &&
                    'border-warning/25 bg-warning-soft/30'
                )}
              >
                <ToolDetails
                  toolContext={context}
                  approvalUi={approvalUi}
                  renderer={renderer}
                  agentChildUi={agentChildUi}
                />
              </div>
            </DetailRow>
          )}
        </div>
      </div>
    </div>
  )
}

function toolIconName(name: string): IconName {
  const lower = name.toLowerCase()
  if (lower.includes('shell') || lower.includes('terminal')) return 'monitor'
  if (lower.includes('approval') || lower.includes('gate')) return 'shield'
  return 'plug'
}

function streamingStatusText({
  gatePending,
  questionnairePending,
  linkedAgentCurrentTool,
  linkedAgentRunning,
  elapsed,
}: {
  gatePending: boolean
  questionnairePending: boolean
  linkedAgentCurrentTool?: string
  linkedAgentRunning: boolean
  elapsed: number
}): string {
  if (gatePending) return '待审批'
  if (questionnairePending) return '待回答'
  if (linkedAgentRunning) {
    return linkedAgentCurrentTool
      ? `子Agent · ${linkedAgentCurrentTool}`
      : '子Agent运行中'
  }
  return runningElapsedLabel(elapsed, 'zh')
}

function ToolCallBlock({
  block,
  sessionId,
  embedded = false,
  defaultOpen = false,
  summaryContent,
  summaryIconName,
}: ToolCallBlockProps) {
  const [isOpen, setIsOpen] = useState(defaultOpen)
  const switchSession = useAppStore((s) => s.switchSession)
  const linkedAgent = useAppStore((state) =>
    block.name === 'agent'
      ? state.agentSessions.find((agent) => agent.toolCallId === block.id)
      : undefined
  )
  const args = toolArgs(block)
  const meta = toolMeta(block)

  const agentSpec =
    block.name === 'agent' && block.argumentsJson
      ? buildStreamingAgentSpec(block.argumentsJson)
      : undefined

  const context: ToolRendererContext = {
    block,
    args,
    meta,
    agentSpec,
  }
  const renderer = getToolRenderer(context)
  const approvalUi =
    block.name === 'askUser' ? (
      <AskUserCard block={block} sessionId={sessionId} args={args} />
    ) : null

  const streaming = block.status === 'streaming'
  const elapsed = useElapsedSeconds(streaming)
  const shellRunningSummary =
    block.name === 'shell' && streaming && !block.text
      ? runningElapsedLabel(elapsed, 'en')
      : null

  const summarySource =
    askUserSummary(block, args) ||
    renderer?.summary?.(context) ||
    block.arguments ||
    block.text ||
    shellRunningSummary ||
    (streaming ? runningElapsedLabel(elapsed, 'zh') : '(无输出)')
  const summaryLine = compactPreviewLine(summarySource)

  const gatePending = block.status === 'streaming' && block.approval != null
  const questionnairePending = isPendingAskUser(block)
  const autoExpand = questionnairePending || gatePending
  const forceOpen = autoExpand
  const open = forceOpen || isOpen

  const displayStatus = streaming
    ? streamingStatusText({
        gatePending,
        questionnairePending,
        linkedAgentCurrentTool: linkedAgent?.currentTool,
        linkedAgentRunning: Boolean(linkedAgent && streaming),
        elapsed,
      })
    : statusLabel(block.status)
  const durationLabel = durationFromMetadata(meta)
  const toolName = block.name || 'tool'
  const summaryIcon = summaryIconName ?? toolIconName(toolName)

  return (
    <details
      className={cn(
        'group mb-1 block min-w-0 text-text-secondary',
        embedded
          ? 'max-w-full overflow-hidden rounded-lg border border-border bg-surface-soft/80 shadow-soft'
          : 'ml-[var(--layout-assistant-indent)] max-w-[760px]'
      )}
      open={open}
      onToggle={(event) => {
        if (forceOpen) {
          if (!event.currentTarget.open) {
            event.currentTarget.open = true
          }
          return
        }
        setIsOpen(event.currentTarget.open)
      }}
    >
      <summary
        className={cn(
          'max-w-full cursor-pointer list-none items-center gap-2 text-[14px] leading-snug text-text-secondary select-none transition-colors duration-150 hover:text-text-primary [&::-webkit-details-marker]:hidden',
          embedded
            ? 'flex px-3 py-1.5 hover:bg-surface-muted/60'
            : 'inline-flex py-1'
        )}
      >
        <Icon name={summaryIcon} size={15} className="shrink-0 opacity-85" />
        {summaryContent ? (
          <span className="min-w-0 overflow-wrap-anywhere">
            {summaryContent}
          </span>
        ) : (
          <span className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap">
            工具调用 {toolName}
          </span>
        )}
        <span className="shrink-0 text-[13px] text-text-muted">
          {block.status === 'complete'
            ? durationLabel || displayStatus
            : displayStatus}
        </span>
        <span className="inline-flex h-4 w-4 shrink-0 items-center justify-center text-text-muted transition-transform duration-150 ease-out group-open:rotate-90">
          <Icon name="chevron-right" size={16} />
        </span>
      </summary>
      {open ? (
        <ToolCallDetailsPanel
          context={context}
          sessionId={sessionId}
          approvalUi={approvalUi}
          renderer={renderer}
          linkedAgent={linkedAgent}
          onOpenChild={(childSessionId) => void switchSession(childSessionId)}
          summaryLine={summaryLine}
          gatePending={gatePending}
        />
      ) : null}
    </details>
  )
}

export default memo(ToolCallBlock)
