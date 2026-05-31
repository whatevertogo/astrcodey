import { useState, useCallback, useMemo } from 'react'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { PHASE_BG_CLASS } from '../../lib/styles'
import { getHostBridge } from '../../lib/hostBridge'
import ProjectGroup from './ProjectGroup'
import NewProjectModal from './NewProjectModal'
import SettingsModal from '../Settings/SettingsModal'
import { Icon } from '../ui'
import * as api from '../../services/api'
import { groupSessionsByWorkingDir } from './projectFolderOrder'

export default function Sidebar() {
  const sessions = useAppStore((s) => s.sessions)
  const projectFolderOrder = useAppStore((s) => s.projectFolderOrder)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const phase = useAppStore((s) => s.phase)
  const workingDir = useAppStore((s) => s.workingDir)
  const createSession = useAppStore((s) => s.createSession)
  const switchSession = useAppStore((s) => s.switchSession)
  const deleteSession = useAppStore((s) => s.deleteSession)
  const deleteProject = useAppStore((s) => s.deleteProject)
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const extensions = useAppStore((s) => s.extensions)
  const refreshExtensionData = useAppStore((s) => s.refreshExtensionData)

  const [showNewProject, setShowNewProject] = useState(false)
  const [showSettings, setShowSettings] = useState(false)

  const bridge = useMemo(() => getHostBridge(), [])
  const projectGroups = useMemo(
    () => groupSessionsByWorkingDir(sessions),
    [sessions]
  )
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
      void switchSession(sessionId)
    },
    [switchSession]
  )

  const handleDeleteSession = useCallback(
    (sessionId: string) => {
      void deleteSession(sessionId)
    },
    [deleteSession]
  )

  const handleDeleteProject = useCallback(
    (wd: string) => {
      void deleteProject(wd)
    },
    [deleteProject]
  )

  const handleNewProject = useCallback(
    async (workingDir: string) => {
      await createSession(workingDir)
      setShowNewProject(false)
    },
    [createSession]
  )

  return (
    <div className="w-full min-w-0 bg-sidebar-bg flex flex-col h-full min-h-0 overflow-hidden px-3 pt-[18px] pb-4">
      <div className="flex items-center gap-2.5 px-2 shrink-0">
        <span
          className={cn(
            'h-[9px] w-[9px] shrink-0 rounded-full shadow-[0_0_0_6px_theme(colors.accent-soft/12%)] transition-[background-color] duration-300 ease-out',
            PHASE_BG_CLASS[phase] ?? PHASE_BG_CLASS.idle
          )}
          title={phase}
        />
        <span className="font-semibold text-[13px] tracking-[0.02em] text-text-primary flex-1">
          AstrCode
        </span>
      </div>

      <div className="mt-4 px-1 flex-shrink-0">
        <button
          type="button"
          onClick={() => {
            const activeWorkingDir =
              workingDir ??
              sessions.find((session) => session.sessionId === activeSessionId)
                ?.workingDir
            if (activeWorkingDir) {
              void createSession(activeWorkingDir)
            } else {
              setShowNewProject(true)
            }
          }}
          className="flex min-h-[34px] w-full items-center gap-2 rounded-lg border-none bg-transparent px-2 text-text-primary outline-none transition-[background-color,color] duration-150 ease-out hover:bg-surface-muted"
        >
          <div className="flex h-4 w-4 shrink-0 items-center justify-center text-text-secondary">
            <Icon name="edit" size={16} />
          </div>
          <span className="truncate text-[13px] font-medium">新会话</span>
        </button>
      </div>

      <div className="flex-1 overflow-y-auto px-1 pt-5 pb-4">
        <div className="px-2 mb-2 text-[11px] font-semibold text-text-muted tracking-[0.05em]">
          文件夹
        </div>
        {orderedWorkingDirs.map((workingDir) => {
          const groupSessions = projectGroups.get(workingDir)
          if (!groupSessions) return null
          return (
            <ProjectGroup
              key={workingDir}
              workingDir={workingDir}
              sessions={groupSessions}
              activeSessionId={activeSessionId}
              onSelectSession={handleSelectSession}
              onDeleteSession={handleDeleteSession}
              onDeleteProject={handleDeleteProject}
            />
          )
        })}
      </div>

      <div className="px-1 pt-4 border-t border-border shrink-0">
        <div className="flex items-center gap-2">
          <button
            type="button"
            className={cn(
              'h-[38px] flex-1 rounded-xl border text-center text-sm font-semibold transition-all duration-150 ease-out hover:-translate-y-px active:translate-y-0 active:scale-[0.98] active:shadow-none',
              showNewProject
                ? 'bg-accent-soft border-accent-strong/20 text-accent-strong shadow-inner translate-y-0 scale-[0.98]'
                : 'bg-surface border-border text-text-primary shadow-soft hover:border-border-strong hover:bg-white'
            )}
            onClick={() => setShowNewProject(true)}
          >
            + 新项目
          </button>
          <button
            type="button"
            className={cn(
              'inline-flex h-[38px] w-[38px] items-center justify-center rounded-xl border transition-all duration-150 ease-out hover:-translate-y-px active:translate-y-0 active:scale-[0.98] active:shadow-none',
              showSettings
                ? 'bg-accent-soft border-accent-strong/20 text-accent-strong shadow-inner translate-y-0 scale-[0.98]'
                : 'bg-surface border-border text-text-secondary shadow-soft hover:border-border-strong hover:bg-white hover:text-text-primary'
            )}
            onClick={() => setShowSettings(true)}
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
      {showSettings && (
        <SettingsModal
          onClose={() => setShowSettings(false)}
          getConfig={api.getConfig}
          reloadConfig={async () => {
            await api.reloadConfig()
          }}
          saveActiveSelection={async (
            profile,
            model,
            smallProfile,
            smallModel,
            approvalMode
          ) => {
            await api.updateActiveSelection(
              profile,
              model,
              smallProfile,
              smallModel,
              approvalMode ?? 'manual'
            )
            bumpModelRefreshKey()
          }}
          testConnection={api.testModel}
          extensions={extensions}
          onRefreshExtensions={refreshExtensionData}
        />
      )}
    </div>
  )
}
