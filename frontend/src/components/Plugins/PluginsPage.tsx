import { useCallback, useEffect, useState } from 'react'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { Button, Icon, IconButton } from '../ui'
import { PageHeader } from '../layout'
import * as api from '../../services/api'
import type { ExtensionStateView } from '../../services/types'

interface PluginsPageProps {
  isSidebarOpen: boolean
  onToggleSidebar: () => void
  onOpenSettings: () => void
}

function sourceLabel(source: ExtensionStateView['source']): string {
  switch (source) {
    case 'builtin':
      return '内置'
    case 'disk':
      return '磁盘'
    default:
      return '未知'
  }
}

function statusLabel(extension: ExtensionStateView): string {
  if (!extension.enabled) return '已禁用'
  if (!extension.loaded) return '未加载'
  return '已加载'
}

function statusClass(extension: ExtensionStateView): string {
  if (!extension.enabled) return 'text-text-muted'
  if (!extension.loaded) return 'text-warning'
  return 'text-success'
}

export default function PluginsPage({
  isSidebarOpen,
  onToggleSidebar,
  onOpenSettings,
}: PluginsPageProps) {
  const extensions = useAppStore((s) => s.extensions)
  const refreshExtensionData = useAppStore((s) => s.refreshExtensionData)
  const [busyExtensionId, setBusyExtensionId] = useState<string | null>(null)
  const [reloading, setReloading] = useState(false)
  const [errorMessage, setErrorMessage] = useState<string | null>(null)

  useEffect(() => {
    void refreshExtensionData()
  }, [refreshExtensionData])

  const handleToggleExtension = useCallback(
    async (extensionId: string, enabled: boolean) => {
      setBusyExtensionId(extensionId)
      setErrorMessage(null)
      try {
        const result = await api.setExtensionEnabled(extensionId, enabled)
        if (result.reloadErrors.length > 0) {
          setErrorMessage(result.reloadErrors.join('; '))
        }
        await refreshExtensionData()
      } catch (err) {
        setErrorMessage(String(err))
      } finally {
        setBusyExtensionId(null)
      }
    },
    [refreshExtensionData]
  )

  const handleReloadExtensions = useCallback(async () => {
    setReloading(true)
    setErrorMessage(null)
    try {
      const result = await api.reloadExtensions()
      if (result.reloadErrors.length > 0) {
        setErrorMessage(result.reloadErrors.join('; '))
      }
      await refreshExtensionData()
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setReloading(false)
    }
  }, [refreshExtensionData])

  const enabledCount = extensions.filter(
    (extension) => extension.enabled
  ).length
  const loadedCount = extensions.filter((extension) => extension.loaded).length

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col overflow-hidden bg-panel-bg">
      <PageHeader>
        <div className="flex min-w-0 items-center gap-2">
          {!isSidebarOpen && (
            <IconButton
              icon="sidebar"
              label="展开侧边栏"
              onClick={onToggleSidebar}
              className="-ml-1"
            />
          )}
          <Icon name="plug" size={18} className="text-text-muted" />
          <span className="truncate text-[14px] font-semibold text-text-primary">
            插件
          </span>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <Button
            variant="ghost"
            className="h-9 px-3 text-[13px]"
            onClick={onOpenSettings}
          >
            设置
          </Button>
          <Button
            variant="secondary"
            onClick={() => void handleReloadExtensions()}
            disabled={reloading}
          >
            {reloading ? '重载中...' : '重载插件'}
          </Button>
        </div>
      </PageHeader>

      <main className="min-h-0 flex-1 overflow-y-auto px-[var(--layout-page-padding-x)] py-8">
        <div className="mx-auto w-full max-w-[980px]">
          <div className="mb-8">
            <h1 className="text-[28px] font-semibold leading-tight text-text-primary">
              插件
            </h1>
            <p className="mt-2 max-w-[680px] text-[14px] leading-relaxed text-text-secondary">
              管理 AstrCode
              的内置和本地插件。插件页独立出来后，扩展状态、重载和启停操作都集中在这里。
            </p>
          </div>

          <div className="mb-6 grid gap-3 sm:grid-cols-3">
            <div className="rounded-lg border border-border bg-surface-soft px-4 py-3">
              <div className="text-[12px] font-medium text-text-muted">
                总数
              </div>
              <div className="mt-1 text-[22px] font-semibold text-text-primary">
                {extensions.length}
              </div>
            </div>
            <div className="rounded-lg border border-border bg-surface-soft px-4 py-3">
              <div className="text-[12px] font-medium text-text-muted">
                已启用
              </div>
              <div className="mt-1 text-[22px] font-semibold text-text-primary">
                {enabledCount}
              </div>
            </div>
            <div className="rounded-lg border border-border bg-surface-soft px-4 py-3">
              <div className="text-[12px] font-medium text-text-muted">
                已加载
              </div>
              <div className="mt-1 text-[22px] font-semibold text-text-primary">
                {loadedCount}
              </div>
            </div>
          </div>

          {errorMessage && (
            <div className="mb-4 rounded-lg border border-danger/20 bg-danger-soft px-4 py-3 text-[13px] text-danger">
              {errorMessage}
            </div>
          )}

          {extensions.length === 0 ? (
            <div className="rounded-lg border border-dashed border-border px-5 py-10 text-center text-[14px] text-text-secondary">
              暂无插件
            </div>
          ) : (
            <div className="grid gap-3">
              {extensions.map((extension) => (
                <div
                  key={extension.extensionId}
                  className="rounded-lg border border-border bg-surface-soft px-4 py-4"
                >
                  <div className="flex min-w-0 items-start justify-between gap-4">
                    <div className="min-w-0">
                      <div className="flex min-w-0 items-center gap-2">
                        <span className="truncate text-[15px] font-semibold text-text-primary">
                          {extension.extensionId}
                        </span>
                        <span
                          className={cn(
                            'shrink-0 text-[12px] font-medium',
                            statusClass(extension)
                          )}
                        >
                          {statusLabel(extension)}
                        </span>
                      </div>
                      <div className="mt-1 text-[12px] text-text-muted">
                        {sourceLabel(extension.source)}
                        {extension.declaration?.capabilities.length
                          ? ` · ${extension.declaration.capabilities.join(', ')}`
                          : ''}
                      </div>
                      {extension.diagnostics?.lastError && (
                        <div className="mt-2 text-[12px] text-danger">
                          {extension.diagnostics.lastError}
                        </div>
                      )}
                    </div>
                    <label className="inline-flex shrink-0 cursor-pointer items-center gap-2 text-[13px] text-text-secondary">
                      <input
                        type="checkbox"
                        checked={extension.enabled}
                        disabled={busyExtensionId === extension.extensionId}
                        onChange={(event) =>
                          void handleToggleExtension(
                            extension.extensionId,
                            event.target.checked
                          )
                        }
                        className="h-4 w-4 accent-accent-strong"
                      />
                      {extension.enabled ? '启用' : '禁用'}
                    </label>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </main>
    </div>
  )
}
