import type { AgentSessionLink } from '../../../services/types'
import { cn } from '../../../lib/utils'

const PHASE_LABELS: Record<string, string> = {
  idle: '就绪',
  thinking: '思考中',
  streaming: '生成中',
  calling_tool: '调用工具',
  compacting: '压缩中',
  error: '错误',
}

interface AgentChildSessionPanelProps {
  agent: AgentSessionLink
  onOpenChild: (childSessionId: string) => void
}

export function AgentChildSessionPanel({
  agent,
  onOpenChild,
}: AgentChildSessionPanelProps) {
  const status = agent.status ?? 'running'
  const phaseLabel =
    agent.phase != null ? (PHASE_LABELS[agent.phase] ?? agent.phase) : null

  return (
    <div className="space-y-2 rounded-lg border border-border bg-surface/60 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[12px]">
        <span className="font-semibold uppercase tracking-wider text-accent">
          子 Agent
        </span>
        <span className="text-text-primary">{agent.agentName ?? '子会话'}</span>
        <span
          className={cn(
            'rounded-full px-2 py-0.5 text-[11px] font-medium',
            status === 'running'
              ? 'bg-accent-soft/20 text-accent'
              : status === 'completed'
                ? 'bg-success/10 text-success'
                : 'bg-danger/10 text-danger'
          )}
        >
          {status === 'running'
            ? '运行中'
            : status === 'completed'
              ? '已完成'
              : '失败'}
        </span>
        {phaseLabel && status === 'running' && (
          <span className="text-text-muted">{phaseLabel}</span>
        )}
        {agent.currentTool && status === 'running' && (
          <span className="font-mono text-text-secondary">
            → {agent.currentTool}
          </span>
        )}
      </div>
      {agent.task && (
        <p className="text-[13px] text-text-secondary">{agent.task}</p>
      )}
      {status === 'running' && (
        <button
          type="button"
          className="rounded-md border border-border px-2.5 py-1 text-[12px] text-text-secondary hover:bg-panel-bg"
          onClick={() => onOpenChild(agent.childSessionId)}
        >
          查看子会话
        </button>
      )}
      {status === 'completed' && agent.summary && (
        <pre className="max-h-48 overflow-auto whitespace-pre-wrap rounded-md bg-code-surface p-2 font-mono text-[12px] text-text-secondary">
          {agent.summary}
        </pre>
      )}
      {status === 'failed' && agent.error && (
        <p className="text-[12px] text-danger">{agent.error}</p>
      )}
    </div>
  )
}
