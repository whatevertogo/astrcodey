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
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const control = useAppStore((s) => s.control)
  const compactSession = useAppStore((s) => s.compactSession)
  const canCompact = Boolean(
    activeSessionId && control?.canRequestCompact && !control.compacting
  )

  return (
    <div className="relative z-30 flex shrink-0 items-center justify-between gap-4 border-b border-border bg-surface/92 px-[22px] py-3.5 backdrop-blur-[12px]">
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
      <button
        type="button"
        className="inline-flex h-8 shrink-0 items-center gap-2 rounded-lg border border-border bg-surface-soft px-3 text-[12px] font-semibold text-text-secondary transition-[background-color,border-color,color,opacity] duration-150 ease-out hover:border-border-strong hover:bg-white hover:text-text-primary disabled:cursor-not-allowed disabled:opacity-40"
        onClick={() => void compactSession()}
        disabled={!canCompact}
        title="压缩上下文"
      >
        压缩
      </button>
    </div>
  )
}
