import { useState, useCallback, useEffect, useRef } from 'react'
import type { SessionListItem } from '../../services/types'
import { cn } from '../../lib/utils'
import SessionItem from './SessionItem'

interface ProjectGroupProps {
  workingDir: string
  sessions: SessionListItem[]
  activeSessionId: string | null
  onSelectSession: (sessionId: string) => void
  onDeleteSession: (sessionId: string) => void
  onDeleteProject: (workingDir: string) => void
}

function ProjectGroup({
  workingDir,
  sessions,
  activeSessionId,
  onSelectSession,
  onDeleteSession,
  onDeleteProject,
}: ProjectGroupProps) {
  const [isExpanded, setIsExpanded] = useState(true)
  const [contextMenu, setContextMenu] = useState<{
    x: number
    y: number
  } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)
  const projectName =
    workingDir.split(/[\\/]/).filter(Boolean).pop() ?? workingDir

  const toggleExpand = useCallback(() => setIsExpanded((v) => !v), [])

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault()
    e.stopPropagation()
    setConfirmDelete(false)
    const x = Math.min(e.clientX, window.innerWidth - 180)
    const y = Math.min(e.clientY, window.innerHeight - 40)
    setContextMenu({ x, y })
  }, [])

  useEffect(() => {
    if (!contextMenu) return
    const handleMouseDown = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setContextMenu(null)
        setConfirmDelete(false)
      }
    }
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setContextMenu(null)
        setConfirmDelete(false)
      }
    }
    document.addEventListener('mousedown', handleMouseDown)
    document.addEventListener('keydown', handleKeyDown)
    return () => {
      document.removeEventListener('mousedown', handleMouseDown)
      document.removeEventListener('keydown', handleKeyDown)
    }
  }, [contextMenu])

  const handleConfirmDelete = useCallback(() => {
    setContextMenu(null)
    setConfirmDelete(false)
    onDeleteProject(workingDir)
  }, [onDeleteProject, workingDir])

  const handleCancelDelete = useCallback(() => {
    setContextMenu(null)
    setConfirmDelete(false)
  }, [])

  const isFolderActive = sessions.some((s) => s.sessionId === activeSessionId)

  return (
    <div className="mb-1">
      <button
        type="button"
        className={cn(
          'flex w-full items-center gap-2 rounded-lg px-2 py-2 text-left outline-none border transition-all duration-150 ease-out',
          isFolderActive
            ? 'bg-accent-soft text-accent-strong border-accent-strong/20 shadow-xs font-semibold'
            : 'border-transparent text-text-secondary hover:bg-surface-muted'
        )}
        onClick={toggleExpand}
        onContextMenu={handleContextMenu}
      >
        <span
          className={cn(
            'inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center transition-colors',
            isFolderActive ? 'text-accent-strong' : 'text-text-secondary'
          )}
        >
          <svg className="h-3.5 w-3.5" viewBox="0 0 20 20">
            <path
              d="M2.5 5.75A1.75 1.75 0 0 1 4.25 4h4.03c.46 0 .9.18 1.23.5l1.02 1c.32.3.74.47 1.18.47h4.04A1.75 1.75 0 0 1 17.5 7.72v6.53A1.75 1.75 0 0 1 15.75 16H4.25A1.75 1.75 0 0 1 2.5 14.25V5.75Z"
              fill="none"
              stroke="currentColor"
              strokeLinejoin="round"
              strokeWidth="1.4"
            />
          </svg>
        </span>
        <span
          className={cn(
            'min-w-0 flex-1 truncate text-[13px]',
            isFolderActive
              ? 'text-accent-strong font-semibold'
              : 'text-text-primary font-medium'
          )}
        >
          {projectName}
        </span>
        <span
          className={cn(
            'inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center transition-all duration-150 ease-out',
            isFolderActive ? 'text-accent-strong' : 'text-text-secondary',
            isExpanded && 'rotate-90'
          )}
        >
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <polyline points="9 18 15 12 9 6"></polyline>
          </svg>
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
      {contextMenu && (
        <div
          ref={menuRef}
          className="fixed z-[100] rounded-xl border border-border bg-surface py-1 shadow-surface-lg"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          {confirmDelete ? (
            <div className="px-3 py-2">
              <div className="mb-2 text-[12px] text-text-secondary">
                删除此项目下的所有会话?
              </div>
              <div className="flex gap-2">
                <button
                  type="button"
                  className="rounded-lg border border-border bg-surface-soft px-2.5 py-1 text-[12px] font-semibold text-text-secondary hover:bg-surface-muted"
                  onClick={handleCancelDelete}
                >
                  取消
                </button>
                <button
                  type="button"
                  className="rounded-lg border border-danger/20 bg-danger-soft px-2.5 py-1 text-[12px] font-semibold text-danger hover:brightness-98"
                  onClick={handleConfirmDelete}
                >
                  删除全部
                </button>
              </div>
            </div>
          ) : (
            <button
              type="button"
              className="flex w-full items-center gap-2 px-3 py-2 text-left text-[13px] text-text-secondary transition-[background-color,color] duration-100 ease-out hover:bg-danger-soft hover:text-danger"
              onClick={() => setConfirmDelete(true)}
            >
              <svg
                className="h-3.5 w-3.5"
                viewBox="0 0 20 20"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.4"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M4 5h12M7 5V3.5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1V5m2 0v10.5a1.5 1.5 0 0 1-1.5 1.5h-7A1.5 1.5 0 0 1 6 15.5V5h8z" />
              </svg>
              删除项目
            </button>
          )}
        </div>
      )}
    </div>
  )
}

export default ProjectGroup
