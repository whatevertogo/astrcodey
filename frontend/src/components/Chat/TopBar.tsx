import { useState, useRef, useEffect } from 'react'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { PHASE_BG_CLASS, ghostIconButton } from '../../lib/styles'

const PHASE_LABELS: Record<string, string> = {
  idle: '就绪',
  thinking: '思考中',
  streaming: '生成中',
  calling_tool: '调用工具',
  compacting: '压缩中',
  error: '错误',
}

interface TopBarProps {
  isSidebarOpen: boolean
  onToggleSidebar: () => void
}

export default function TopBar({
  isSidebarOpen,
  onToggleSidebar,
}: TopBarProps) {
  const phase = useAppStore((s) => s.phase)
  const activeSessionTitle = useAppStore((s) => s.activeSessionTitle)
  const agentSessions = useAppStore((s) => s.agentSessions)
  const switchSession = useAppStore((s) => s.switchSession)

  const [agentMenuOpen, setAgentMenuOpen] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!agentMenuOpen) return
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setAgentMenuOpen(false)
      }
    }
    document.addEventListener('mousedown', handler)
    return () => document.removeEventListener('mousedown', handler)
  }, [agentMenuOpen])

  return (
    <div className="relative z-30 flex shrink-0 items-center gap-4 border-b border-border bg-surface/92 px-[22px] py-3.5 backdrop-blur-[12px]">
      <div className="flex items-center gap-1.5 min-w-0">
        <button
          className={cn(ghostIconButton, '-ml-1 p-1')}
          type="button"
          onClick={onToggleSidebar}
          aria-label={isSidebarOpen ? '收起侧边栏' : '展开侧边栏'}
          title={isSidebarOpen ? '收起侧边栏' : '展开侧边栏'}
        >
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <rect x="3" y="3" width="18" height="18" rx="2" ry="2" />
            <line x1="9" y1="3" x2="9" y2="21" />
          </svg>
        </button>
        <span
          className={cn(
            'h-[9px] w-[9px] shrink-0 rounded-full shadow-[0_0_0_6px_theme(colors.accent-soft/12%)] transition-[background-color] duration-300 ease-out',
            PHASE_BG_CLASS[phase] ?? PHASE_BG_CLASS.idle
          )}
          title={phase}
        />
        <span className="min-w-0 truncate text-[13px] font-semibold text-text-primary">
          {activeSessionTitle || 'AstrCode'}
        </span>
        {phase !== 'idle' && (
          <span className="shrink-0 text-xs text-text-secondary">
            {PHASE_LABELS[phase] ?? phase}
          </span>
        )}
      </div>
      {agentSessions.length > 0 && (
        <div ref={menuRef} className="relative ml-auto shrink-0">
          <button
            type="button"
            className="inline-flex items-center gap-1 rounded-full bg-accent-soft/20 px-2 py-0.5 text-xs font-medium text-accent hover:bg-accent-soft/30"
            onClick={() => setAgentMenuOpen((v) => !v)}
            aria-expanded={agentMenuOpen}
            aria-haspopup="true"
          >
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" />
              <circle cx="9" cy="7" r="4" />
              <path d="M22 21v-2a4 4 0 0 0-3-3.87" />
              <path d="M16 3.13a4 4 0 0 1 0 7.75" />
            </svg>
            {agentSessions.length} agent{agentSessions.length > 1 ? 's' : ''}
          </button>
          {agentMenuOpen && (
            <div className="absolute right-0 top-full z-50 mt-1 min-w-[220px] rounded-lg border border-border bg-surface p-2 shadow-lg">
              {agentSessions.map((agent) => (
                <button
                  key={agent.childSessionId}
                  type="button"
                  className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs hover:bg-accent-soft/10"
                  onClick={() => {
                    switchSession(agent.childSessionId)
                    setAgentMenuOpen(false)
                  }}
                >
                  <span className="font-medium text-text-primary">{agent.agentName}</span>
                  <span className="min-w-0 flex-1 truncate text-text-secondary">{agent.task}</span>
                </button>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  )
}
