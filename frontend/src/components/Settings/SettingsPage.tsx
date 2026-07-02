import { useCallback, useEffect, useMemo, useState } from 'react'
import { useAppStore } from '../../store/conversation'
import { btnPrimary, fieldInput } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { getStoredTheme, setTheme, type ThemePreference } from '../../lib/theme'
import { Button, Icon, IconButton, Modal } from '../ui'
import { PageHeader } from '../layout'
import * as api from '../../services/api'
import type {
  ConfigView,
  ModelTestResult,
  ProfileView,
  ProviderAuthScheme,
  ProviderSpecView,
  ProviderWireFormat,
} from '../../services/types'

type SettingsSection = 'model' | 'appearance'

interface ProviderConfigDialogState {
  provider: ProviderSpecView
  existingProfile?: ProfileView
  baseUrl: string
  apiKey: string
  modelId: string
}

interface ProviderRemoveDialogState {
  provider: ProviderSpecView
  profile: ProfileView
}

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

function wireFormatLabel(wireFormat: ProviderWireFormat): string {
  switch (wireFormat) {
    case 'openai_chat_completions':
      return 'OpenAI Chat'
    case 'openai_responses':
      return 'OpenAI Responses'
    case 'anthropic_messages':
      return 'Anthropic Messages'
    case 'google_genai':
      return 'Google GenAI'
  }
}

function wireLabel(profile: ProfileView | undefined): string {
  return profile ? wireFormatLabel(profile.wireFormat) : ''
}

function authSchemeLabel(authScheme: ProviderAuthScheme): string {
  switch (authScheme) {
    case 'none':
      return 'No auth'
    case 'bearer':
      return 'Bearer'
    case 'x_api_key':
      return 'x-api-key'
    case 'x_goog_api_key':
      return 'x-goog-api-key'
  }
}

function authLabel(profile: ProfileView | undefined): string {
  return profile ? authSchemeLabel(profile.authScheme) : ''
}

function findProviderProfile(
  profiles: ProfileView[],
  provider: ProviderSpecView
): ProfileView | undefined {
  return (
    profiles.find((profile) => profile.name === provider.id) ??
    profiles.find(
      (profile) =>
        profile.providerKind === provider.providerKind &&
        profile.wireFormat === provider.wireFormat
    )
  )
}

export default function SettingsPage({
  isSidebarOpen,
  onToggleSidebar,
  onOpenPlugins,
}: SettingsPageProps) {
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const [section, setSection] = useState<SettingsSection>('model')
  const [configView, setConfigView] = useState<ConfigView | null>(null)
  const [providerCatalog, setProviderCatalog] = useState<ProviderSpecView[]>([])
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
  const [applyingProviderId, setApplyingProviderId] = useState<string | null>(
    null
  )
  const [removingProviderId, setRemovingProviderId] = useState<string | null>(
    null
  )
  const [activatingProviderId, setActivatingProviderId] = useState<
    string | null
  >(null)
  const [providerConfigDialog, setProviderConfigDialog] =
    useState<ProviderConfigDialogState | null>(null)
  const [providerRemoveDialog, setProviderRemoveDialog] =
    useState<ProviderRemoveDialogState | null>(null)

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
    Promise.all([api.getConfig(), api.getProviderCatalog()])
      .then(([config, catalog]) => {
        if (cancelled) return
        applyConfig(config)
        setProviderCatalog(catalog.providers)
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
  const catalogProviders = useMemo(() => providerCatalog, [providerCatalog])
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

  const openProviderConfigDialog = useCallback(
    (provider: ProviderSpecView, existingProfile?: ProfileView) => {
      const defaultEndpoint = provider.endpoints.find(
        (endpoint) => endpoint.isDefault
      )
      const modelId =
        existingProfile?.models.find((model) => model.id === selectedModel)
          ?.id ??
        existingProfile?.models[0]?.id ??
        provider.defaultModel

      setProviderConfigDialog({
        provider,
        existingProfile,
        baseUrl: existingProfile?.baseUrl ?? defaultEndpoint?.baseUrl ?? '',
        apiKey: '',
        modelId,
      })
      setStatusMessage(null)
      setErrorMessage(null)
      setTestResult(null)
    },
    [selectedModel]
  )

  const handleApplyProviderPreset = useCallback(
    async (dialog: ProviderConfigDialogState, activate = true) => {
      const baseUrl = dialog.baseUrl.trim()
      const apiKey = dialog.apiKey.trim()
      const modelId = dialog.modelId.trim() || dialog.provider.defaultModel
      if (!baseUrl) return

      setApplyingProviderId(dialog.provider.id)
      setStatusMessage(null)
      setErrorMessage(null)
      setTestResult(null)
      try {
        const response = await api.applyProviderPreset({
          providerId: dialog.provider.id,
          baseUrl,
          apiKey: apiKey || undefined,
          modelId,
          activate,
        })
        applyConfig(await api.getConfig())
        if (activate) {
          bumpModelRefreshKey()
        }
        setProviderConfigDialog(null)
        setStatusMessage(
          response.warning
            ? `已保存 ${response.profileName}；${response.warning}`
            : response.activated
              ? `已应用 ${response.profileName}`
              : `已保存 ${response.profileName}`
        )
      } catch (err) {
        setErrorMessage(String(err))
      } finally {
        setApplyingProviderId(null)
      }
    },
    [applyConfig, bumpModelRefreshKey]
  )

  const handleActivateProviderProfile = useCallback(
    async (provider: ProviderSpecView, profile: ProfileView) => {
      const modelId = profile.models[0]?.id
      if (!modelId) return

      setActivatingProviderId(provider.id)
      setStatusMessage(null)
      setErrorMessage(null)
      setTestResult(null)
      try {
        const response = await api.updateActiveSelection(
          profile.name,
          modelId,
          selectedSmallProfile || undefined,
          selectedSmallModel || undefined,
          yoloEnabled ? 'yolo' : 'manual'
        )
        applyConfig(await api.getConfig())
        bumpModelRefreshKey()
        setStatusMessage(
          response.warning
            ? `已切换到 ${profile.name}；${response.warning}`
            : `已切换到 ${profile.name}`
        )
      } catch (err) {
        setErrorMessage(String(err))
      } finally {
        setActivatingProviderId(null)
      }
    },
    [
      applyConfig,
      bumpModelRefreshKey,
      selectedSmallModel,
      selectedSmallProfile,
      yoloEnabled,
    ]
  )

  const handleRemoveProviderPreset = useCallback(
    async (dialog: ProviderRemoveDialogState) => {
      setRemovingProviderId(dialog.provider.id)
      setStatusMessage(null)
      setErrorMessage(null)
      setTestResult(null)
      try {
        const response = await api.removeProviderPreset(dialog.profile.name)
        applyConfig(await api.getConfig())
        bumpModelRefreshKey()
        setProviderRemoveDialog(null)
        setStatusMessage(
          response.warning
            ? `已取消 ${response.removedProfileName}；${response.warning}`
            : `已取消 ${response.removedProfileName}`
        )
      } catch (err) {
        setErrorMessage(String(err))
      } finally {
        setRemovingProviderId(null)
      }
    },
    [applyConfig, bumpModelRefreshKey]
  )

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
                          {profile.name} · {wireLabel(profile)}
                        </option>
                      ))}
                    </select>
                    {currentProfile && (
                      <div className="mt-2 text-[12px] text-text-tertiary">
                        {wireLabel(currentProfile)} ·{' '}
                        {authLabel(currentProfile)}
                      </div>
                    )}
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
                          {profile.name} · {wireLabel(profile)}
                        </option>
                      ))}
                    </select>
                    {currentSmallProfile && (
                      <div className="mt-2 text-[12px] text-text-tertiary">
                        {wireLabel(currentSmallProfile)} ·{' '}
                        {authLabel(currentSmallProfile)}
                      </div>
                    )}
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

                {catalogProviders.length > 0 && (
                  <div className="space-y-3">
                    <div className="flex items-center justify-between gap-4">
                      <h2 className="text-[13px] font-semibold text-text-secondary">
                        Provider Catalog
                      </h2>
                      <span className="text-[12px] text-text-tertiary">
                        {catalogProviders.length} presets
                      </span>
                    </div>
                    <div className="grid gap-3 lg:grid-cols-2">
                      {catalogProviders.map((provider) => {
                        const defaultEndpoint = provider.endpoints.find(
                          (endpoint) => endpoint.isDefault
                        )
                        const existingProviderProfile = findProviderProfile(
                          profiles,
                          provider
                        )
                        const isConfigured = Boolean(existingProviderProfile)
                        const isActive =
                          existingProviderProfile?.name === selectedProfile
                        const configuredModel =
                          existingProviderProfile?.models.find(
                            (model) => model.id === selectedModel
                          )?.id ?? existingProviderProfile?.models[0]?.id
                        const displayedBaseUrl =
                          existingProviderProfile?.baseUrl ??
                          defaultEndpoint?.baseUrl ??
                          defaultEndpoint?.label ??
                          '-'
                        const isProviderBusy =
                          applyingProviderId === provider.id ||
                          removingProviderId === provider.id ||
                          activatingProviderId === provider.id
                        const canConfigure =
                          !applyingProviderId &&
                          !removingProviderId &&
                          !activatingProviderId &&
                          !saving &&
                          !reloading &&
                          !testing
                        const canActivate =
                          Boolean(existingProviderProfile && configuredModel) &&
                          !isActive &&
                          canConfigure
                        const canRemove =
                          Boolean(existingProviderProfile) &&
                          canConfigure
                        const capabilityLabels = [
                          provider.capabilities.promptCacheKey
                            ? 'Cache key'
                            : null,
                          provider.capabilities.streamUsage
                            ? 'Stream usage'
                            : null,
                          provider.capabilities.reasoningEffort
                            ? 'Reasoning'
                            : null,
                        ].filter((label): label is string => Boolean(label))
                        return (
                          <div
                            key={provider.id}
                            className="min-w-0 rounded-lg border border-border bg-surface-soft px-4 py-3"
                          >
                            <div className="flex min-w-0 items-start justify-between gap-3">
                              <div className="min-w-0">
                                <div className="truncate text-[14px] font-semibold text-text-primary">
                                  {provider.displayName}
                                </div>
                                <div className="mt-1 truncate text-[12px] text-text-tertiary">
                                  {provider.providerKind} ·{' '}
                                  {authSchemeLabel(provider.authScheme)}
                                </div>
                              </div>
                              <span className="shrink-0 rounded-md border border-border bg-panel-bg px-2 py-1 text-[11px] text-text-secondary">
                                {wireFormatLabel(provider.wireFormat)}
                              </span>
                            </div>
                            <div className="mt-3 divide-y divide-border text-[12px]">
                              <div className="flex justify-between gap-3 py-1.5">
                                <span className="shrink-0 text-text-tertiary">
                                  状态
                                </span>
                                <span
                                  className={cn(
                                    'min-w-0 text-right font-medium',
                                    isActive
                                      ? 'text-success'
                                      : isConfigured
                                        ? 'text-text-primary'
                                        : 'text-text-tertiary'
                                  )}
                                >
                                  {isActive
                                    ? '当前使用'
                                    : isConfigured
                                      ? '已配置'
                                      : '未配置'}
                                </span>
                              </div>
                              <div className="flex justify-between gap-3 py-1.5">
                                <span className="shrink-0 text-text-tertiary">
                                  Model
                                </span>
                                <span className="min-w-0 break-all text-right text-text-primary">
                                  {configuredModel ?? provider.defaultModel}
                                </span>
                              </div>
                              <div className="flex justify-between gap-3 py-1.5">
                                <span className="shrink-0 text-text-tertiary">
                                  Base URL
                                </span>
                                <span className="min-w-0 break-all text-right text-text-primary">
                                  {displayedBaseUrl}
                                </span>
                              </div>
                              <div className="flex justify-between gap-3 py-1.5">
                                <span className="shrink-0 text-text-tertiary">
                                  API Key
                                </span>
                                <span className="min-w-0 break-all text-right text-text-primary">
                                  {existingProviderProfile?.hasApiKey
                                    ? '已配置'
                                    : provider.apiKeyEnvVars[0]
                                      ? `env:${provider.apiKeyEnvVars[0]}`
                                      : '未配置'}
                                </span>
                              </div>
                            </div>
                            {capabilityLabels.length > 0 && (
                              <div className="mt-3 flex flex-wrap gap-1.5">
                                {capabilityLabels.map((label) => (
                                  <span
                                    key={label}
                                    className="rounded-md bg-surface-muted px-2 py-1 text-[11px] text-text-secondary"
                                  >
                                    {label}
                                  </span>
                                ))}
                              </div>
                            )}
                            <div className="mt-3 flex flex-wrap justify-between gap-2">
                              {existingProviderProfile ? (
                                <Button
                                  variant="danger"
                                  className="h-8 px-3 text-[12px]"
                                  disabled={!canRemove}
                                  onClick={() => {
                                    setProviderRemoveDialog({
                                      provider,
                                      profile: existingProviderProfile,
                                    })
                                  }}
                                >
                                  移除配置
                                </Button>
                              ) : (
                                <span />
                              )}
                              <div className="flex flex-wrap justify-end gap-2">
                                {existingProviderProfile && !isActive && (
                                  <Button
                                    variant="secondary"
                                    className="h-8 px-3 text-[12px]"
                                    disabled={!canActivate}
                                    onClick={() =>
                                      void handleActivateProviderProfile(
                                        provider,
                                        existingProviderProfile
                                      )
                                    }
                                  >
                                    {activatingProviderId === provider.id
                                      ? '切换中...'
                                      : '设为当前'}
                                  </Button>
                                )}
                                <Button
                                  variant="secondary"
                                  className="h-8 px-3 text-[12px]"
                                  disabled={!canConfigure || isProviderBusy}
                                  onClick={() =>
                                    openProviderConfigDialog(
                                      provider,
                                      existingProviderProfile
                                    )
                                  }
                                >
                                  {isConfigured ? '编辑配置' : '配置'}
                                </Button>
                              </div>
                            </div>
                          </div>
                        )
                      })}
                    </div>
                  </div>
                )}

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
      {providerConfigDialog && (
        <Modal
          title={`${providerConfigDialog.existingProfile ? '编辑' : '配置'} ${providerConfigDialog.provider.displayName}`}
          onClose={() => {
            if (applyingProviderId) return
            setProviderConfigDialog(null)
          }}
          className="w-[520px]"
        >
          <form
            className="space-y-4"
            onSubmit={(event) => {
              event.preventDefault()
              void handleApplyProviderPreset(providerConfigDialog, true)
            }}
          >
            <div>
              <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                Base URL
              </label>
              <input
                className={fieldInput}
                value={providerConfigDialog.baseUrl}
                placeholder="https://api.example.com/v1"
                disabled={Boolean(applyingProviderId)}
                onChange={(event) => {
                  const baseUrl = event.target.value
                  setProviderConfigDialog((current) =>
                    current ? { ...current, baseUrl } : current
                  )
                  setStatusMessage(null)
                  setErrorMessage(null)
                }}
              />
            </div>

            <div>
              <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                API Key
              </label>
              <input
                className={fieldInput}
                type="password"
                value={providerConfigDialog.apiKey}
                placeholder={
                  providerConfigDialog.existingProfile?.hasApiKey
                    ? '已配置，留空保留'
                    : providerConfigDialog.provider.apiKeyEnvVars[0]
                      ? `API Key 或 env:${providerConfigDialog.provider.apiKeyEnvVars[0]}`
                      : 'API Key'
                }
                disabled={Boolean(applyingProviderId)}
                onChange={(event) => {
                  const apiKey = event.target.value
                  setProviderConfigDialog((current) =>
                    current ? { ...current, apiKey } : current
                  )
                  setStatusMessage(null)
                  setErrorMessage(null)
                }}
              />
              {providerConfigDialog.existingProfile?.hasApiKey && (
                <div className="mt-2 text-[12px] text-text-tertiary">
                  已保存的 Key 不会显示。
                </div>
              )}
            </div>

            <div>
              <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
                Model
              </label>
              <input
                className={fieldInput}
                value={providerConfigDialog.modelId}
                placeholder={providerConfigDialog.provider.defaultModel}
                disabled={Boolean(applyingProviderId)}
                onChange={(event) => {
                  const modelId = event.target.value
                  setProviderConfigDialog((current) =>
                    current ? { ...current, modelId } : current
                  )
                  setStatusMessage(null)
                  setErrorMessage(null)
                }}
              />
            </div>

            <div className="flex justify-end gap-2 pt-2">
              <Button
                variant="secondary"
                disabled={Boolean(applyingProviderId)}
                onClick={() => setProviderConfigDialog(null)}
              >
                取消
              </Button>
              <Button
                variant="secondary"
                disabled={
                  Boolean(applyingProviderId) ||
                  !providerConfigDialog.baseUrl.trim()
                }
                onClick={() =>
                  void handleApplyProviderPreset(providerConfigDialog, false)
                }
              >
                仅保存
              </Button>
              <Button
                variant="primary"
                disabled={
                  Boolean(applyingProviderId) ||
                  !providerConfigDialog.baseUrl.trim()
                }
                type="submit"
              >
                {applyingProviderId ? '保存中...' : '保存并使用'}
              </Button>
            </div>
          </form>
        </Modal>
      )}

      {providerRemoveDialog && (
        <Modal
          title={`移除 ${providerRemoveDialog.provider.displayName} 配置`}
          onClose={() => {
            if (removingProviderId) return
            setProviderRemoveDialog(null)
          }}
          className="w-[460px]"
        >
          <div className="space-y-4">
            <p className="text-[13px] leading-relaxed text-text-secondary">
              这会删除 {providerRemoveDialog.profile.name} 的 Base URL、API
              Key 和模型配置。
            </p>
            {providerRemoveDialog.profile.name === selectedProfile && (
              <p className="rounded-lg border border-warning/20 bg-warning-soft px-3 py-2 text-[13px] text-warning">
                当前正在使用这个 Provider，移除后会切换到其他可用配置。
              </p>
            )}
            <div className="flex justify-end gap-2 pt-2">
              <Button
                variant="secondary"
                disabled={Boolean(removingProviderId)}
                onClick={() => setProviderRemoveDialog(null)}
              >
                返回
              </Button>
              <Button
                variant="danger"
                disabled={Boolean(removingProviderId)}
                onClick={() => void handleRemoveProviderPreset(providerRemoveDialog)}
              >
                {removingProviderId ? '移除中...' : '移除配置'}
              </Button>
            </div>
          </div>
        </Modal>
      )}
    </div>
  )
}
