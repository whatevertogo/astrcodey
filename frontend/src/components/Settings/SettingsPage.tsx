import { useCallback, useEffect, useMemo, useState } from 'react'
import { useAppStore } from '../../store/conversation'
import { btnPrimary, fieldInput } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { getStoredTheme, setTheme, type ThemePreference } from '../../lib/theme'
import { Button, Icon, IconButton } from '../ui'
import { PageHeader } from '../layout'
import * as api from '../../services/api'
import type {
  ConfigView,
  ModelTestResult,
  ProfileView,
} from '../../services/types'

type SettingsSection = 'model' | 'appearance'

const THEME_OPTIONS: { value: ThemePreference; label: string; hint: string }[] =
  [
    { value: 'dark', label: '深色', hint: '适合长时间编码和低光环境' },
    { value: 'light', label: '浅色', hint: '适合明亮环境和文档阅读' },
    { value: 'system', label: '跟随系统', hint: '自动匹配系统外观设置' },
  ]

interface SettingsPageProps {
  isSidebarOpen: boolean
  onToggleSidebar: () => void
  onOpenPlugins: () => void
}

function pickModel(
  profile: ProfileView | undefined,
  currentModel: string
): string {
  if (!profile || profile.models.length === 0) return ''
  if (profile.models.some((model) => model.id === currentModel)) {
    return currentModel
  }
  return profile.models[0]?.id ?? ''
}

export default function SettingsPage({
  isSidebarOpen,
  onToggleSidebar,
  onOpenPlugins,
}: SettingsPageProps) {
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const [section, setSection] = useState<SettingsSection>('model')
  const [configView, setConfigView] = useState<ConfigView | null>(null)
  const [selectedProfile, setSelectedProfile] = useState('')
  const [selectedModel, setSelectedModel] = useState('')
  const [selectedSmallProfile, setSelectedSmallProfile] = useState('')
  const [selectedSmallModel, setSelectedSmallModel] = useState('')
  const [yoloEnabled, setYoloEnabled] = useState(false)
  const [themePreference, setThemePreference] =
    useState<ThemePreference>(getStoredTheme)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [reloading, setReloading] = useState(false)
  const [testing, setTesting] = useState(false)
  const [testResult, setTestResult] = useState<ModelTestResult | null>(null)
  const [statusMessage, setStatusMessage] = useState<string | null>(null)
  const [errorMessage, setErrorMessage] = useState<string | null>(null)

  const applyConfig = useCallback((config: ConfigView) => {
    setConfigView(config)
    setSelectedProfile(config.activeProfile)
    setSelectedModel(config.activeModel)
    setSelectedSmallProfile(config.activeSmallProfile ?? '')
    setSelectedSmallModel(config.activeSmallModel ?? '')
    setYoloEnabled(config.approvalMode === 'yolo')
  }, [])

  useEffect(() => {
    let cancelled = false
    api
      .getConfig()
      .then((config) => {
        if (cancelled) return
        applyConfig(config)
      })
      .catch((err) => {
        if (!cancelled) setErrorMessage(String(err))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [applyConfig])

  const profiles = useMemo(() => configView?.profiles ?? [], [configView])
  const currentProfile = useMemo(
    () => profiles.find((profile) => profile.name === selectedProfile),
    [profiles, selectedProfile]
  )
  const currentSmallProfile = useMemo(
    () => profiles.find((profile) => profile.name === selectedSmallProfile),
    [profiles, selectedSmallProfile]
  )

  const handleSave = useCallback(async () => {
    if (!selectedProfile || !selectedModel) return
    setSaving(true)
    setStatusMessage(null)
    setErrorMessage(null)
    try {
      await api.updateActiveSelection(
        selectedProfile,
        selectedModel,
        selectedSmallProfile || undefined,
        selectedSmallModel || undefined,
        yoloEnabled ? 'yolo' : 'manual'
      )
      bumpModelRefreshKey()
      setStatusMessage('已保存')
      const config = await api.getConfig()
      setConfigView(config)
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setSaving(false)
    }
  }, [
    bumpModelRefreshKey,
    selectedModel,
    selectedProfile,
    selectedSmallModel,
    selectedSmallProfile,
    yoloEnabled,
  ])

  const handleReload = useCallback(async () => {
    setReloading(true)
    setStatusMessage(null)
    setErrorMessage(null)
    try {
      await api.reloadConfig()
      applyConfig(await api.getConfig())
      setTestResult(null)
      setStatusMessage('已从磁盘重载')
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setReloading(false)
    }
  }, [applyConfig])

  const handleTest = useCallback(async () => {
    setTesting(true)
    setStatusMessage(null)
    setErrorMessage(null)
    setTestResult(null)
    try {
      setTestResult(await api.testModel())
    } catch (err) {
      setTestResult({ success: false, message: String(err) })
    } finally {
      setTesting(false)
    }
  }, [])

  const handleThemeChange = useCallback((preference: ThemePreference) => {
    setThemePreference(preference)
    setTheme(preference)
  }, [])

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
          <Icon name="settings" size={18} className="text-text-muted" />
          <span className="truncate text-[14px] font-semibold text-text-primary">
            设置
          </span>
        </div>
        <div className="ml-auto">
          <Button
            variant="ghost"
            className="h-9 px-3 text-[13px]"
            onClick={onOpenPlugins}
          >
            插件
          </Button>
        </div>
      </PageHeader>

      <main className="min-h-0 flex-1 overflow-y-auto px-[var(--layout-page-padding-x)] py-8">
        <div className="mx-auto grid w-full max-w-[980px] gap-8 lg:grid-cols-[200px_minmax(0,1fr)]">
          <aside className="min-w-0">
            <h1 className="text-[28px] font-semibold leading-tight text-text-primary">
              设置
            </h1>
            <p className="mt-2 text-[14px] leading-relaxed text-text-secondary">
              调整模型、权限和界面外观。
            </p>
            <div className="mt-6 space-y-1">
              {[
                { id: 'model' as const, label: '模型与权限' },
                { id: 'appearance' as const, label: '外观' },
              ].map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className={cn(
                    'flex min-h-9 w-full items-center rounded-lg px-3 text-left text-[14px] font-medium transition-colors',
                    section === item.id
                      ? 'bg-surface-muted text-text-primary'
                      : 'text-text-secondary hover:bg-surface-muted hover:text-text-primary'
                  )}
                  onClick={() => setSection(item.id)}
                >
                  {item.label}
                </button>
              ))}
            </div>
          </aside>

          <section className="min-w-0">
            {loading ? (
              <div className="flex items-center gap-2 text-[13px] text-text-secondary">
                <span className="h-4 w-4 animate-spin rounded-full border-2 border-border border-t-text-secondary" />
                加载设置...
              </div>
            ) : section === 'model' ? (
              <div className="space-y-5">
                <div>
                  <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                    配置文件
                  </label>
                  <div className="overflow-hidden text-ellipsis whitespace-nowrap rounded-lg border border-border bg-surface-soft px-3 py-2.5 text-[12px] text-text-primary">
                    {configView?.configPath ?? ''}
                  </div>
                </div>

                <div className="grid gap-4 md:grid-cols-2">
                  <div>
                    <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                      Profile
                    </label>
                    <select
                      className={fieldInput}
                      value={selectedProfile}
                      onChange={(event) => {
                        const profileName = event.target.value
                        const profile = profiles.find(
                          (item) => item.name === profileName
                        )
                        setSelectedProfile(profileName)
                        setSelectedModel(pickModel(profile, selectedModel))
                        setTestResult(null)
                        setStatusMessage(null)
                        setErrorMessage(null)
                      }}
                    >
                      {profiles.map((profile) => (
                        <option key={profile.name} value={profile.name}>
                          {profile.name}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div>
                    <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                      Model
                    </label>
                    <select
                      className={fieldInput}
                      value={selectedModel}
                      disabled={!currentProfile?.models.length}
                      onChange={(event) => {
                        setSelectedModel(event.target.value)
                        setTestResult(null)
                        setStatusMessage(null)
                        setErrorMessage(null)
                      }}
                    >
                      {currentProfile?.models.map((model) => (
                        <option key={model.id} value={model.id}>
                          {model.id}
                        </option>
                      ))}
                    </select>
                  </div>
                </div>

                <div className="grid gap-4 md:grid-cols-2">
                  <div>
                    <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                      Small Profile
                    </label>
                    <select
                      className={fieldInput}
                      value={selectedSmallProfile}
                      onChange={(event) => {
                        const profileName = event.target.value
                        const profile = profiles.find(
                          (item) => item.name === profileName
                        )
                        setSelectedSmallProfile(profileName)
                        setSelectedSmallModel(
                          profileName && profile?.models.length
                            ? profile.models[0].id
                            : ''
                        )
                        setTestResult(null)
                        setStatusMessage(null)
                        setErrorMessage(null)
                      }}
                    >
                      <option value="">不使用</option>
                      {profiles.map((profile) => (
                        <option key={profile.name} value={profile.name}>
                          {profile.name}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div>
                    <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                      Small Model
                    </label>
                    <select
                      className={fieldInput}
                      value={selectedSmallModel}
                      disabled={!currentSmallProfile?.models.length}
                      onChange={(event) => {
                        setSelectedSmallModel(event.target.value)
                        setTestResult(null)
                        setStatusMessage(null)
                        setErrorMessage(null)
                      }}
                    >
                      {!currentSmallProfile && <option value="">不使用</option>}
                      {currentSmallProfile?.models.map((model) => (
                        <option key={model.id} value={model.id}>
                          {model.id}
                        </option>
                      ))}
                    </select>
                  </div>
                </div>

                <div className="rounded-lg border border-border bg-surface-soft px-4 py-4">
                  <div className="flex items-center justify-between gap-4">
                    <div className="min-w-0">
                      <div className="text-[14px] font-semibold text-text-primary">
                        工具权限
                      </div>
                      <div className="mt-1 text-[13px] leading-relaxed text-text-secondary">
                        开启后自动批准工具调用；关闭时保留手动确认。
                      </div>
                    </div>
                    <label className="inline-flex shrink-0 cursor-pointer items-center gap-2 text-[13px] text-text-secondary">
                      <input
                        type="checkbox"
                        checked={yoloEnabled}
                        onChange={(event) => {
                          setYoloEnabled(event.target.checked)
                          setStatusMessage(null)
                          setErrorMessage(null)
                        }}
                        className="h-4 w-4 accent-accent-strong"
                      />
                      {yoloEnabled ? '完全访问' : '手动确认'}
                    </label>
                  </div>
                </div>

                <div className="divide-y divide-border rounded-lg border border-border">
                  <div className="flex justify-between gap-4 px-4 py-3">
                    <span className="text-[13px] text-text-secondary">
                      Base URL
                    </span>
                    <span className="break-all text-right text-[13px] text-text-primary">
                      {currentProfile?.baseUrl ?? '-'}
                    </span>
                  </div>
                  <div className="flex justify-between gap-4 px-4 py-3">
                    <span className="text-[13px] text-text-secondary">
                      API Key
                    </span>
                    <span className="text-[13px] text-text-primary">
                      {currentProfile?.hasApiKey ? '已配置' : '未配置'}
                    </span>
                  </div>
                </div>

                <div className="flex flex-wrap justify-end gap-2.5">
                  <Button
                    variant="secondary"
                    onClick={() => void handleReload()}
                    disabled={reloading || saving || testing}
                  >
                    {reloading ? '重载中...' : '从磁盘重载'}
                  </Button>
                  <Button
                    variant="secondary"
                    onClick={() => void handleTest()}
                    disabled={testing || saving}
                  >
                    {testing ? '测试中...' : '测试连接'}
                  </Button>
                  <button
                    type="button"
                    className={btnPrimary}
                    onClick={() => void handleSave()}
                    disabled={saving || testing}
                  >
                    {saving ? '保存中...' : '保存'}
                  </button>
                </div>
              </div>
            ) : (
              <div className="divide-y divide-border rounded-lg border border-border">
                {THEME_OPTIONS.map((option) => (
                  <label
                    key={option.value}
                    className="flex cursor-pointer items-center justify-between gap-4 px-4 py-4 transition-colors hover:bg-surface-muted"
                  >
                    <span className="min-w-0">
                      <span className="block text-[14px] font-medium text-text-primary">
                        {option.label}
                      </span>
                      <span className="mt-1 block text-[13px] text-text-muted">
                        {option.hint}
                      </span>
                    </span>
                    <input
                      type="radio"
                      name="theme"
                      value={option.value}
                      checked={themePreference === option.value}
                      onChange={() => handleThemeChange(option.value)}
                      className="h-4 w-4 shrink-0 accent-accent-strong"
                    />
                  </label>
                ))}
              </div>
            )}

            {testResult && (
              <div
                className={cn(
                  'mt-4 rounded-lg border px-4 py-3 text-[13px]',
                  testResult.success
                    ? 'border-success/20 bg-success-soft text-success'
                    : 'border-danger/20 bg-danger-soft text-danger'
                )}
              >
                {testResult.success ? '连接成功' : '连接失败'}:{' '}
                {testResult.message}
              </div>
            )}
            {statusMessage && (
              <div className="mt-4 rounded-lg border border-success/20 bg-success-soft px-4 py-3 text-[13px] text-success">
                {statusMessage}
              </div>
            )}
            {errorMessage && (
              <div className="mt-4 rounded-lg border border-danger/20 bg-danger-soft px-4 py-3 text-[13px] text-danger">
                {errorMessage}
              </div>
            )}
          </section>
        </div>
      </main>
    </div>
  )
}
