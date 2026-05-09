import { memo, useState, useCallback, useEffect, useRef } from 'react'
import type { SessionListItem } from '../../services/types'
import { cn } from '../../lib/utils'
import { PHASE_BG_CLASS } from '../../lib/styles'

interface SessionItemProps {
  session: SessionListItem
  isActive: boolean
  onSelect: (sessionId: string) => void
  onDelete: (sessionId: string) => void
}

function SessionItem({
  session,
  isActive,
  onSelect,
  onDelete,
}: SessionItemProps) {
  const [contextMenu, setContextMenu] = useState<{
    x: number
    y: number
  } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault()
    setConfirmDelete(false)
    setContextMenu({ x: e.clientX, y: e.clientY })
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

  const handleRequestDelete = useCallback(() => {
    setConfirmDelete(true)
  }, [])

  const handleConfirmDelete = useCallback(() => {
    setContextMenu(null)
    setConfirmDelete(false)
    onDelete(session.sessionId)
  }, [onDelete, session.sessionId])

  const handleCancelDelete = useCallback(() => {
    setContextMenu(null)
    setConfirmDelete(false)
  }, [])

  return (
    <>
      <button
        type="button"
        className={cn(
          'flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left outline-none transition-[background-color] duration-150 ease-out hover:bg-black/5',
          isActive && 'bg-black/5'
        )}
        onClick={() => onSelect(session.sessionId)}
        onContextMenu={handleContextMenu}
      >
        <span
          className={cn(
            'h-2 w-2 shrink-0 rounded-full transition-[background-color] duration-300 ease-out',
            PHASE_BG_CLASS[session.phase] ?? PHASE_BG_CLASS.idle
          )}
        />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] text-text-primary">
            {session.firstUserMessage || '新会话'}
          </div>
          <div className="truncate text-[11px] text-text-muted">
            {session.workingDir}
          </div>
        </div>
      </button>
      {contextMenu && (
        <div
          ref={menuRef}
          className="fixed z-[100] rounded-xl border border-border bg-surface py-1 shadow-surface-lg"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          {confirmDelete ? (
            <div className="px-3 py-2">
              <div className="mb-2 text-[12px] text-text-secondary">
                确认删除此会话?
              </div>
              <div className="flex gap-2">
                <button
                  type="button"
                  className="rounded-lg border border-border bg-surface-soft px-2.5 py-1 text-[12px] font-semibold text-text-secondary hover:bg-white"
                  onClick={handleCancelDelete}
                >
                  取消
                </button>
                <button
                  type="button"
                  className="rounded-lg border border-danger/20 bg-danger-soft px-2.5 py-1 text-[12px] font-semibold text-danger hover:brightness-98"
                  onClick={handleConfirmDelete}
                >
                  删除
                </button>
              </div>
            </div>
          ) : (
            <button
              type="button"
              className="flex w-full items-center gap-2 px-3 py-2 text-left text-[13px] text-text-secondary transition-[background-color,color] duration-100 ease-out hover:bg-danger-soft hover:text-danger"
              onClick={handleRequestDelete}
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
              删除会话
            </button>
          )}
        </div>
      )}
    </>
  )
}

export default memo(SessionItem)
