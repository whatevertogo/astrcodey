import { useState, useCallback } from 'react';
import type { SessionListItem } from '../../services/types';
import { cn } from '../../lib/utils';
import SessionItem from './SessionItem';

interface ProjectGroupProps {
  workingDir: string;
  sessions: SessionListItem[];
  activeSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  onDeleteSession: (sessionId: string) => void;
}

function ProjectGroup({ workingDir, sessions, activeSessionId, onSelectSession, onDeleteSession }: ProjectGroupProps) {
  const [isExpanded, setIsExpanded] = useState(true);
  const projectName = workingDir.split(/[\\/]/).filter(Boolean).pop() ?? workingDir;

  const toggleExpand = useCallback(() => setIsExpanded((v) => !v), []);

  return (
    <div className="mb-1">
      <button
        type="button"
        className="flex w-full items-center gap-2 rounded-lg px-2 py-2 text-left outline-none transition-[background-color] duration-150 ease-out hover:bg-black/5"
        onClick={toggleExpand}
      >
        <span className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-text-secondary">
          <svg className="h-3.5 w-3.5" viewBox="0 0 20 20">
            <path d="M2.5 5.75A1.75 1.75 0 0 1 4.25 4h4.03c.46 0 .9.18 1.23.5l1.02 1c.32.3.74.47 1.18.47h4.04A1.75 1.75 0 0 1 17.5 7.72v6.53A1.75 1.75 0 0 1 15.75 16H4.25A1.75 1.75 0 0 1 2.5 14.25V5.75Z" fill="none" stroke="currentColor" strokeLinejoin="round" strokeWidth="1.4" />
          </svg>
        </span>
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-text-primary">{projectName}</span>
        <span className={cn(
          'inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-text-secondary transition-transform duration-150 ease-out',
          isExpanded && 'rotate-90'
        )}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="9 18 15 12 9 6"></polyline></svg>
        </span>
      </button>
      {isExpanded && (
        <div className="ml-2 flex flex-col">
          {sessions.map((session) => (
            <SessionItem
              key={session.sessionId}
              session={session}
              isActive={activeSessionId === session.sessionId}
              onSelect={onSelectSession}
              onDelete={onDeleteSession}
            />
          ))}
        </div>
      )}
    </div>
  );
}

export default ProjectGroup;
