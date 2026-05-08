import { useAppStore } from '../../store/conversation';
import { cn } from '../../lib/utils';
import { PHASE_BG_CLASS } from '../../lib/styles';

const PHASE_LABELS: Record<string, string> = {
  idle: '就绪',
  thinking: '思考中',
  streaming: '生成中',
  calling_tool: '调用工具',
  compacting: '压缩中',
  error: '错误',
};

export default function TopBar() {
  const phase = useAppStore((s) => s.phase);
  const activeSessionTitle = useAppStore((s) => s.activeSessionTitle);
  const activeSessionId = useAppStore((s) => s.activeSessionId);

  return (
    <div className="relative z-30 flex shrink-0 items-center justify-between gap-4 border-b border-border bg-surface/92 px-[22px] py-3.5 backdrop-blur-[12px]">
      <div className="flex items-center gap-2.5 min-w-0">
        <span
          className={cn(
            'h-[9px] w-[9px] shrink-0 rounded-full shadow-[0_0_0_6px_theme(colors.accent-soft/12%)] transition-[background-color] duration-300 ease-out',
            PHASE_BG_CLASS[phase] ?? PHASE_BG_CLASS.idle
          )}
          title={phase}
        />
        <span className="min-w-0 truncate text-[13px] font-semibold text-text-primary">
          {activeSessionTitle || (activeSessionId ? 'AstrCode' : 'AstrCode')}
        </span>
        {phase !== 'idle' && (
          <span className="shrink-0 text-xs text-text-secondary">
            {PHASE_LABELS[phase] ?? phase}
          </span>
        )}
      </div>
    </div>
  );
}
