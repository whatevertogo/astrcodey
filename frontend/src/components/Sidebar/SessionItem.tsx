import { memo } from 'react';
import type { SessionListItem } from '../../services/types';
import { cn } from '../../lib/utils';
import { PHASE_BG_CLASS } from '../../lib/styles';

interface SessionItemProps {
  session: SessionListItem;
  isActive: boolean;
  onSelect: (sessionId: string) => void;
}

function SessionItem({ session, isActive, onSelect }: SessionItemProps) {
  return (
    <button
      type="button"
      className={cn(
        'flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left outline-none transition-[background-color] duration-150 ease-out hover:bg-black/5',
        isActive && 'bg-black/5'
      )}
      onClick={() => onSelect(session.sessionId)}
    >
      <span
        className={cn(
          'h-2 w-2 shrink-0 rounded-full transition-[background-color] duration-300 ease-out',
          PHASE_BG_CLASS[session.phase] ?? PHASE_BG_CLASS.idle
        )}
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13px] text-text-primary">{session.title}</div>
        <div className="truncate text-[11px] text-text-muted">{session.workingDir}</div>
      </div>
    </button>
  );
}

export default memo(SessionItem);
