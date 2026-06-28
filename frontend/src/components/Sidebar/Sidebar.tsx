import { useState, useCallback, useMemo } from 'react'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { getHostBridge } from '../../lib/hostBridge'
import NewProjectModal from './NewProjectModal'
import { Icon } from '../ui'
import { groupSessionsByWorkingDir } from './projectFolderOrder'
import type { MainView } from '../../App'
import type { IconName } from '../ui/Icon'
import type { SessionListItem } from '../../services/types'

const NAV_ITEMS: Array<{
  icon: IconName
  label: string
  disabled?: boolean
}> = [
  { icon: 'edit', label: '新对话' },
  { icon: 'plug', label: '插件' },
]

function projectNameFromDir(workingDir: string): string {
  return workingDir.split(/[\\/]/).filter(Boolean).pop() ?? workingDir
}

function formatSessionAge(updatedAt: string): string {
  const updated = new Date(updatedAt).getTime()
  if (!Number.isFinite(updated)) return ''

  const diffMs = Date.now() - updated
  const minute = 60 * 1000
  const hour = 60 * minute
  const day = 24 * hour

  if (diffMs < hour) return '刚刚'
  if (diffMs < day) return `${Math.max(1, Math.floor(diffMs / hour))} 小时`
  return `${Math.max(1, Math.floor(diffMs / day))} 天`
}

function sortByUpdatedDesc(sessions: SessionListItem[]): SessionListItem[] {
  return [...sessions].sort((left, right) => {
    const leftTime = new Date(left.updatedAt).getTime()
    const rightTime = new Date(right.updatedAt).getTime()
    return rightTime - leftTime
  })
}

interface SidebarProps {
  activeView: MainView
  onToggleSidebar: () => void
  onOpenChat: () => void
  onOpenPlugins: () => void
  onOpenSettings: () => void
}

export default function Sidebar({
  activeView,
  onToggleSidebar,
  onOpenChat,
  onOpenPlugins,
  onOpenSettings,
}: SidebarProps) {
  const sessions = useAppStore((s) => s.sessions)
  const projectFolderOrder = useAppStore((s) => s.projectFolderOrder)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const phase = useAppStore((s) => s.phase)
  const workingDir = useAppStore((s) => s.workingDir)
  const createSession = useAppStore((s) => s.createSession)
  const switchSession = useAppStore((s) => s.switchSession)

  const [showNewProject, setShowNewProject] = useState(false)

  const bridge = useMemo(() => getHostBridge(), [])
  const projectGroups = useMemo(
    () => groupSessionsByWorkingDir(sessions),
    [sessions]
  )
  const activeWorkingDir =
    workingDir ??
    sessions.find((session) => session.sessionId === activeSessionId)
      ?.workingDir ??
    null
  const orderedWorkingDirs = useMemo(() => {
    const active = new Set(projectGroups.keys())
    const ordered = projectFolderOrder.filter((workingDir) =>
      active.has(workingDir)
    )
    for (const workingDir of active) {
      if (!ordered.includes(workingDir)) {
        ordered.push(workingDir)
      }
    }
    return ordered
  }, [projectFolderOrder, projectGroups])

  const handleSelectSession = useCallback(
    (sessionId: string) => {
      onOpenChat()
      void switchSession(sessionId)
    },
    [onOpenChat, switchSession]
  )

  const handleNewProject = useCallback(
    async (workingDir: string) => {
      await createSession(workingDir)
      onOpenChat()
      setShowNewProject(false)
    },
    [createSession, onOpenChat]
  )

  const handleCreateConversation = useCallback(() => {
    const recentSession = sortByUpdatedDesc(sessions)[0]
    const currentWorkingDir =
      activeWorkingDir ?? recentSession?.workingDir ?? null
    if (currentWorkingDir) {
      onOpenChat()
      void createSession(currentWorkingDir)
    } else {
      setShowNewProject(true)
    }
  }, [activeWorkingDir, createSession, onOpenChat, sessions])

  return (
    <div className="flex h-full min-h-0 w-full min-w-0 flex-col overflow-hidden bg-sidebar-bg text-text-secondary">
      <div className="flex h-14 shrink-0 items-center gap-3 px-2">
        <button
          type="button"
          className="inline-flex h-7 w-24 items-center justify-center rounded-full bg-[#6d58ff] text-white shadow-[0_8px_22px_rgba(109,88,255,0.25)] transition-transform duration-150 active:scale-[0.98]"
          aria-label="AstrCode"
          title={`AstrCode ${phase}`}
        >
          <Icon name="monitor" size={18} />
        </button>
        <button
          type="button"
          className="ml-auto inline-flex h-8 w-8 items-center justify-center rounded-lg text-text-muted transition-colors hover:bg-surface-muted hover:text-text-primary"
          onClick={onToggleSidebar}
          aria-label="切换边栏"
          title="边栏"
        >
          <Icon name="sidebar" size={17} />
        </button>
      </div>

      <div className="shrink-0 px-3 pt-2">
        <div className="space-y-1">
          {NAV_ITEMS.map((item) => {
            const isPlugins = item.label === '插件'
            const isNewConversation = item.label === '新对话'
            return (
              <button
                key={item.label}
                type="button"
                disabled={item.disabled}
                className={cn(
                  'flex min-h-10 w-full items-center gap-3 rounded-lg px-3 text-left text-[15px] font-semibold outline-none transition-colors duration-150',
                  isPlugins && activeView === 'plugins'
                    ? 'bg-surface-muted text-text-primary'
                    : 'text-text-primary hover:bg-surface-muted',
                  item.disabled &&
                    'cursor-default opacity-70 hover:bg-transparent'
                )}
                onClick={() => {
                  if (isNewConversation) handleCreateConversation()
                  if (isPlugins) onOpenPlugins()
                }}
                title={item.disabled ? '即将支持' : item.label}
              >
                <Icon
                  name={item.icon}
                  size={18}
                  className={item.disabled ? 'text-text-muted' : undefined}
                />
                <span className="truncate">{item.label}</span>
              </button>
            )
          })}
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-5 pt-8">
        <div className="mb-3 flex items-center justify-between px-3">
          <div className="text-[14px] font-semibold text-text-muted">项目</div>
          <button
            type="button"
            className="inline-flex h-7 w-7 items-center justify-center rounded-lg text-text-muted transition-colors hover:bg-surface-muted hover:text-text-primary"
            onClick={() => setShowNewProject(true)}
            aria-label="新项目"
            title="新项目"
          >
            <Icon name="plus" size={16} />
          </button>
        </div>
        <div className="space-y-1">
          {orderedWorkingDirs.map((dir) => {
            const groupSessions = projectGroups.get(dir)
            const orderedSessions = groupSessions
              ? sortByUpdatedDesc(groupSessions)
              : []
            const latestSession = orderedSessions[0]
            const isActive = dir === activeWorkingDir
            return (
              <div key={dir} className="mb-2">
                <button
                  type="button"
                  className={cn(
                    'flex min-h-9 w-full items-center gap-3 rounded-lg px-3 text-left text-[15px] font-medium outline-none transition-colors duration-150',
                    isActive && activeView === 'chat'
                      ? 'bg-surface-muted text-text-primary'
                      : 'text-text-secondary hover:bg-surface-muted hover:text-text-primary'
                  )}
                  onClick={() => {
                    if (latestSession)
                      handleSelectSession(latestSession.sessionId)
                  }}
                  title={dir}
                >
                  <Icon name="project" size={16} className="shrink-0" />
                  <span className="truncate">{projectNameFromDir(dir)}</span>
                </button>

                <div className="mt-1 space-y-0.5 pl-7">
                  {orderedSessions.map((session) => {
                    const isSessionActive =
                      session.sessionId === activeSessionId
                    return (
                      <button
                        key={session.sessionId}
                        type="button"
                        className={cn(
                          'grid min-h-8 w-full grid-cols-[minmax(0,1fr)_auto] items-center gap-3 rounded-lg px-2.5 text-left text-[14px] outline-none transition-colors duration-150',
                          isSessionActive && activeView === 'chat'
                            ? 'bg-surface-muted text-text-primary'
                            : 'text-text-secondary hover:bg-surface-muted hover:text-text-primary'
                        )}
                        onClick={() => handleSelectSession(session.sessionId)}
                        title={
                          session.title || session.firstUserMessage || '新对话'
                        }
                      >
                        <span className="truncate font-medium">
                          {session.firstUserMessage ||
                            session.title ||
                            '新对话'}
                        </span>
                        <span className="shrink-0 text-[12px] text-text-muted">
                          {formatSessionAge(session.updatedAt)}
                        </span>
                      </button>
                    )
                  })}
                </div>
              </div>
            )
          })}
          {orderedWorkingDirs.length === 0 && (
            <div className="px-3 py-2 text-[13px] text-text-muted">
              暂无项目
            </div>
          )}
        </div>
      </div>

      <div className="shrink-0 border-t border-border px-3 py-3">
        <div className="flex min-w-0 items-center justify-between gap-3">
          <span className="truncate px-2 text-[13px] font-medium text-text-muted">
            AstrCode
          </span>
          <button
            type="button"
            className={cn(
              'inline-flex h-9 w-9 items-center justify-center rounded-lg transition-colors hover:bg-surface-muted hover:text-text-primary',
              activeView === 'settings'
                ? 'bg-surface-muted text-text-primary'
                : 'text-text-muted'
            )}
            onClick={onOpenSettings}
            aria-label="打开设置"
            title="设置"
          >
            <Icon name="settings" size={16} />
          </button>
        </div>
      </div>

      {showNewProject && (
        <NewProjectModal
          onConfirm={handleNewProject}
          onCancel={() => setShowNewProject(false)}
          canBrowse={bridge.canSelectDirectory}
          onSelectDirectory={bridge.selectDirectory}
        />
      )}
    </div>
  )
}
