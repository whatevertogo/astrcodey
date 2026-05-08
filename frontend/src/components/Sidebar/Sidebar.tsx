import { useState, useCallback, useMemo } from 'react'
import { useAppStore } from '../../store/conversation'
import type { SessionListItem } from '../../services/types'
import { cn } from '../../lib/utils'
import { PHASE_BG_CLASS } from '../../lib/styles'
import { getHostBridge } from '../../lib/hostBridge'
import ProjectGroup from './ProjectGroup'
import NewProjectModal from './NewProjectModal'
import SettingsModal from '../Settings/SettingsModal'
import * as api from '../../services/api'

function groupByWorkingDir(
  sessions: SessionListItem[]
): Map<string, SessionListItem[]> {
  const groups = new Map<string, SessionListItem[]>()
  for (const session of sessions) {
    const existing = groups.get(session.workingDir)
    if (existing) {
      existing.push(session)
    } else {
      groups.set(session.workingDir, [session])
    }
  }
  return groups
}

export default function Sidebar() {
  const sessions = useAppStore((s) => s.sessions)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const phase = useAppStore((s) => s.phase)
  const workingDir = useAppStore((s) => s.workingDir)
  const createSession = useAppStore((s) => s.createSession)
  const switchSession = useAppStore((s) => s.switchSession)
  const deleteSession = useAppStore((s) => s.deleteSession)
  const deleteProject = useAppStore((s) => s.deleteProject)
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)

  const [showNewProject, setShowNewProject] = useState(false)
  const [showSettings, setShowSettings] = useState(false)

  const bridge = useMemo(() => getHostBridge(), [])
  const projectGroups = useMemo(() => groupByWorkingDir(sessions), [sessions])

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
          className="flex min-h-[34px] w-full items-center gap-2 rounded-lg border-none bg-transparent px-2 text-text-primary outline-none transition-[background-color,color] duration-150 ease-out hover:bg-black/5"
        >
          <div className="w-4 h-4 flex items-center justify-center shrink-0 text-text-secondary">
            <svg
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="w-4 h-4"
            >
              <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"></path>
              <path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"></path>
            </svg>
          </div>
          <span className="truncate text-[13px] font-medium">新会话</span>
        </button>
      </div>

      <div className="flex-1 overflow-y-auto px-1 pt-5 pb-4">
        <div className="px-2 mb-2 text-[11px] font-semibold text-text-muted tracking-[0.05em]">
          文件夹
        </div>
        {Array.from(projectGroups.entries()).map(
          ([workingDir, groupSessions]) => (
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
        )}
      </div>

      <div className="px-1 pt-4 border-t border-border shrink-0">
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="h-[38px] flex-1 rounded-xl border border-border bg-surface text-center text-sm font-semibold text-text-primary shadow-soft transition-[background-color,border-color,transform] duration-150 ease-out hover:border-border-strong hover:bg-white hover:-translate-y-px"
            onClick={() => setShowNewProject(true)}
          >
            + 新项目
          </button>
          <button
            type="button"
            className="inline-flex h-[38px] w-[38px] items-center justify-center rounded-xl border border-border bg-surface text-text-secondary shadow-soft transition-[background-color,color,border-color,transform] duration-150 ease-out hover:border-border-strong hover:bg-white hover:text-text-primary hover:-translate-y-px"
            onClick={() => setShowSettings(true)}
            aria-label="打开设置"
            title="设置"
          >
            <svg viewBox="0 0 24 24" className="w-4 h-4" aria-hidden="true">
              <path
                d="M10.4 2h3.2l.5 2.6c.6.2 1.1.5 1.6.9l2.5-.9 1.6 2.8-2 1.7c.1.3.1.6.1.9s0 .6-.1.9l2 1.7-1.6 2.8-2.5-.9c-.5.4-1 .7-1.6.9l-.5 2.6h-3.2l-.5-2.6c-.6-.2-1.1-.5-1.6-.9l-2.5.9-1.6-2.8 2-1.7c-.1-.3-.1-.6-.1-.9s0-.6.1-.9l-2-1.7 1.6-2.8 2.5.9c.5-.4 1-.7 1.6-.9L10.4 2Zm1.6 6.5A3.5 3.5 0 1 0 12 15.5 3.5 3.5 0 0 0 12 8.5Z"
                fill="currentColor"
              />
            </svg>
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
          saveActiveSelection={async (profile, model) => {
            await api.updateActiveSelection(profile, model)
            bumpModelRefreshKey()
          }}
          testConnection={api.testModel}
        />
      )}
    </div>
  )
}
