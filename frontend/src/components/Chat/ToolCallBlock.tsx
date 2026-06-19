import { memo, useState, type ReactNode } from 'react'
import {
  useElapsedSeconds,
  runningElapsedLabel,
} from '../../hooks/useElapsedSeconds'
import type { ConversationBlock } from '../../services/types'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import {
  extractRenderSpec,
  extractRenderSummary,
} from '../../types/render-spec'
import {
  renderToolApprovalUi,
  toolApprovalShouldAutoExpand,
  toolApprovalSummary,
  toolApprovalPending,
  type ToolUiContext,
} from '../../tool-ui'
import { GateApprovalCard } from '../../tool-ui/components/GateApprovalCard'
import { readGateApproval } from '../../tool-ui/components/gateApprovalMeta'
import {
  chevronIcon,
  toolPanelPaddingX,
  toolPanelScrollViewport,
} from '../../lib/styles'
import { RenderSpecViewer } from './RenderSpecViewer'
import './tools/builtinRenderers'
import {
  getToolRenderer,
  type ToolRenderer,
  type ToolRendererContext,
} from './tools/registry'
import { compactLine, statusLabel, toolArgs, toolMeta } from './tools/helpers'
import { DefaultToolDetails, StatusIndicatorDot } from './tools/shared'
import { buildStreamingAgentSpec } from './tools/agentSpec'
import { AgentChildSessionPanel } from './tools/AgentChildSessionPanel'
import { Icon } from '../ui/Icon'

interface ToolCallBlockProps {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
  sessionId: string | null
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
  if (toolContext.renderSpec) {
    return (
      <div className="space-y-3">
        <RenderSpecViewer spec={toolContext.renderSpec} />
        {agentChildUi}
      </div>
    )
  }
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

function ToolCallBlock({ block, sessionId }: ToolCallBlockProps) {
  const [isOpen, setIsOpen] = useState(false)
  const agentSessions = useAppStore((s) => s.agentSessions)
  const switchSession = useAppStore((s) => s.switchSession)
  const args = toolArgs(block)
  const meta = toolMeta(block)

  const renderSpec = extractRenderSpec(block.metadata)
  const agentSpec =
    block.name === 'agent' && block.argumentsJson && !renderSpec
      ? buildStreamingAgentSpec(block.argumentsJson)
      : undefined

  const toolUiCtx: ToolUiContext = {
    block,
    sessionId,
    args,
    meta,
    renderSpec,
  }
  const approvalUi = renderToolApprovalUi(toolUiCtx)

  const context: ToolRendererContext = {
    block,
    args,
    meta,
    renderSpec,
    agentSpec,
  }
  const renderer = getToolRenderer(context)

  const streaming = block.status === 'streaming'
  const elapsed = useElapsedSeconds(streaming && !block.text)
  const shellRunningSummary =
    block.name === 'shell' && streaming && !block.text
      ? runningElapsedLabel(elapsed, 'en')
      : null

  const summaryLine = compactLine(
    extractRenderSummary(block.metadata) ||
      toolApprovalSummary(toolUiCtx) ||
      renderer?.summary?.(context) ||
      block.arguments ||
      block.text ||
      shellRunningSummary ||
      (streaming ? runningElapsedLabel(elapsed, 'zh') : '(无输出)')
  )

  const gateApproval = readGateApproval(block.metadata)
  const gatePending = gateApproval?.pending === true
  const questionnairePending = toolApprovalPending(toolUiCtx)
  const linkedAgent =
    block.name === 'agent'
      ? agentSessions.find((agent) => agent.toolCallId === block.id)
      : undefined
  const agentChildUi =
    linkedAgent && block.status === 'streaming' ? (
      <AgentChildSessionPanel
        agent={linkedAgent}
        onOpenChild={(childSessionId) => void switchSession(childSessionId)}
      />
    ) : null
  const autoExpand =
    toolApprovalShouldAutoExpand(toolUiCtx) || gatePending || !!agentChildUi

  const displayStatus = gatePending
    ? '待审批'
    : questionnairePending
      ? '待回答'
      : linkedAgent && block.status === 'streaming'
        ? linkedAgent.currentTool
          ? `子Agent · ${linkedAgent.currentTool}`
          : '子Agent运行中'
        : streaming
          ? runningElapsedLabel(elapsed, 'zh')
          : statusLabel(block.status)

  return (
    <details
      className="group mb-1 ml-[var(--layout-assistant-indent)] block min-w-0 max-w-full animate-block-enter motion-reduce:animate-none"
      open={block.status === 'error' || isOpen || !!agentSpec || autoExpand}
      onToggle={(e) => setIsOpen(e.currentTarget.open)}
    >
      <summary className="flex min-w-0 cursor-pointer list-none items-center gap-3 py-2 font-mono text-[13px] leading-relaxed text-text-secondary select-none hover:opacity-90 [&::-webkit-details-marker]:hidden">
        <span className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-border bg-surface px-2 py-0.5 font-mono text-[11px] font-semibold uppercase tracking-wider text-text-secondary">
          <StatusIndicatorDot
            status={block.status}
            pendingApproval={gatePending}
          />
          {block.name}
        </span>
        <span
          className="block min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap font-mono text-[12.5px] text-text-secondary/85 opacity-90"
          title={summaryLine}
        >
          {summaryLine}
        </span>
        <span
          className={cn(
            'shrink-0 text-[11px] font-semibold uppercase tracking-wider',
            gatePending
              ? 'text-warning'
              : questionnairePending
                ? 'text-accent'
                : 'text-text-muted'
          )}
        >
          {displayStatus}
        </span>
        <span className={chevronIcon}>
          <Icon name="chevron-right" size={14} />
        </span>
      </summary>
      <div className="mt-1.5 min-w-0 overflow-hidden rounded-xl border border-border bg-code-surface shadow-soft">
        <div className={toolPanelScrollViewport}>
          <div className={cn(toolPanelPaddingX, 'py-3')}>
            {gatePending && sessionId ? (
              <GateApprovalCard
                sessionId={sessionId}
                callId={block.id}
                toolName={block.name}
                metadata={block.metadata}
                args={args}
              />
            ) : (
              <ToolDetails
                toolContext={context}
                approvalUi={approvalUi}
                renderer={renderer}
                agentChildUi={agentChildUi}
              />
            )}
          </div>
        </div>
      </div>
    </details>
  )
}

export default memo(ToolCallBlock)
