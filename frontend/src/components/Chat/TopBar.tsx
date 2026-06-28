import { useState } from 'react'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { PHASE_BG_CLASS } from '../../lib/styles'
import { PageHeader } from '../layout'
import { Dropdown, Icon, IconButton } from '../ui'

const PHASE_LABELS: Record<string, string> = {
  idle: '就绪',
  thinking: '思考中',
  streaming: '生成中',
  calling_tool: '调用工具',
  compacting: '压缩中',
  error: '错误',
}

const STATUS_LABELS: Record<string, string> = {
  running: '运行中',
  completed: '已完成',
  failed: '失败',
}

interface TopBarProps {
  isSidebarOpen: boolean
  onToggleSidebar: () => void
}

function statusDotClass(status: string | undefined): string {
  switch (status) {
    case 'running':
      return 'text-accent'
    case 'completed':
      return 'text-success'
    case 'failed':
      return 'text-danger'
    default:
      return 'text-text-muted'
  }
}

export default function TopBar({
  isSidebarOpen,
  onToggleSidebar,
}: TopBarProps) {
  const phase = useAppStore((s) => s.phase)
  const activeSessionTitle = useAppStore((s) => s.activeSessionTitle)
  const agentSessions = useAppStore((s) => s.agentSessions)
  const switchSession = useAppStore((s) => s.switchSession)

  const [subsessionMenuOpen, setSubsessionMenuOpen] = useState(false)

  return (
    <PageHeader>
      <div className="flex min-w-0 items-center gap-1.5">
        {!isSidebarOpen && (
          <IconButton
            icon="sidebar"
            label="展开侧边栏"
            onClick={onToggleSidebar}
            className="-ml-1"
          />
        )}
        <span
          className={cn(
            'h-[9px] w-[9px] shrink-0 rounded-full opacity-70 shadow-[0_0_0_6px_theme(colors.accent-soft/12%)] transition-[background-color] duration-300 ease-out',
            isSidebarOpen && 'sr-only',
            PHASE_BG_CLASS[phase] ?? PHASE_BG_CLASS.idle
          )}
          title={phase}
          aria-hidden="true"
        />
        <span className="min-w-0 truncate text-[13px] font-semibold text-text-primary">
          {isSidebarOpen ? '' : activeSessionTitle || 'AstrCode'}
        </span>
        {phase !== 'idle' && (
          <span className="shrink-0 text-xs text-text-secondary">
            {PHASE_LABELS[phase] ?? phase}
          </span>
        )}
      </div>

      {agentSessions.length > 0 && (
        <div className="relative ml-auto shrink-0">
          <Dropdown
            open={subsessionMenuOpen}
            onClose={() => setSubsessionMenuOpen(false)}
            label="子会话"
            align="right"
            trigger={
              <button
                type="button"
                className="inline-flex items-center gap-1 rounded-full bg-accent-soft/20 px-2 py-0.5 text-xs font-medium text-accent hover:bg-accent-soft/30"
                onClick={() => setSubsessionMenuOpen((v) => !v)}
                aria-expanded={subsessionMenuOpen}
                aria-haspopup="menu"
              >
                <Icon name="users" size={12} />
                {agentSessions.length} 个子会话
              </button>
            }
          >
            {agentSessions.map((agent) => (
              <button
                key={agent.childSessionId}
                type="button"
                className="flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left text-xs hover:bg-accent-soft/10"
                role="menuitem"
                onClick={() => {
                  switchSession(agent.childSessionId)
                  setSubsessionMenuOpen(false)
                }}
              >
                <span
                  className={statusDotClass(agent.status)}
                  aria-hidden="true"
                >
                  ●
                </span>
                <span className="min-w-0 flex-1">
                  <span className="flex min-w-0 items-center gap-2">
                    <span className="truncate font-medium text-text-primary">
                      {agent.agentName || '子会话'}
                    </span>
                    <span className="shrink-0 text-[11px] text-text-secondary">
                      {agent.status === 'running' && agent.phase
                        ? PHASE_LABELS[agent.phase]
                        : STATUS_LABELS[agent.status ?? 'running']}
                    </span>
                  </span>
                  <span className="block truncate text-text-secondary">
                    {agent.task || ''}
                  </span>
                  {agent.status === 'running' && agent.currentTool && (
                    <span className="block truncate text-[11px] text-text-secondary">
                      {agent.currentTool}
                    </span>
                  )}
                  {agent.status === 'completed' && agent.summary && (
                    <span className="block truncate text-[11px] text-text-secondary">
                      {agent.summary}
                    </span>
                  )}
                  {agent.status === 'failed' && agent.error && (
                    <span className="block truncate text-[11px] text-danger">
                      {agent.error}
                    </span>
                  )}
                </span>
              </button>
            ))}
          </Dropdown>
        </div>
      )}
    </PageHeader>
  )
}
