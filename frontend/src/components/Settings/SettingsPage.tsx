import { useCallback, useEffect, useMemo, useState } from 'react'
import { useAppStore } from '../../store/conversation'
import { fieldInput } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { getStoredTheme, setTheme, type ThemePreference } from '../../lib/theme'
import { Button, Icon, IconButton, Modal, type IconName } from '../ui'
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

type SettingsSection = 'models' | 'providers' | 'permissions' | 'appearance'

const SETTINGS_NAV_ITEMS: {
  id: SettingsSection
  label: string
  hint: string
  icon: IconName
}[] = [
  { id: 'models', label: '模型', hint: '当前主模型与小模型', icon: 'settings' },
  {
    id: 'providers',
    label: 'Providers',
    hint: '所有已配置和预设',
    icon: 'plug',
  },
  { id: 'permissions', label: '权限', hint: '工具批准策略', icon: 'shield' },
  { id: 'appearance', label: '外观', hint: '主题显示偏好', icon: 'monitor' },
]

const settingsPanelClass =
  'overflow-hidden rounded-lg border border-border bg-surface-soft'
const settingsRowClass =
  'flex min-w-0 flex-col items-stretch justify-between gap-3 px-4 py-3 sm:flex-row sm:items-center sm:gap-4'
const settingsDividerClass = 'divide-y divide-border'
const quietButtonClass =
  'inline-flex h-8 items-center justify-center rounded-md border border-border-strong bg-surface px-3 text-[12px] font-medium text-text-primary transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-40'
const settingsPrimaryButtonClass =
  'inline-flex h-8 items-center justify-center rounded-md border border-border-strong bg-btn-primary-bg px-3 text-[12px] font-medium text-btn-primary-fg transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40'
const settingsDangerButtonClass =
  'inline-flex h-8 items-center justify-center rounded-md border border-danger/30 bg-danger-soft px-3 text-[12px] font-medium text-danger transition-colors hover:brightness-110 disabled:cursor-not-allowed disabled:opacity-40'
const compactPillClass =
  'inline-flex min-h-6 shrink-0 items-center rounded-md border border-border bg-panel-bg px-2 text-[11px] font-medium text-text-secondary'

interface ProviderConfigDialogState {
  provider: ProviderSpecView
  existingProfile?: ProfileView
  baseUrl: string
  apiKey: string
  modelId: string
}

interface ProviderRemoveDialogState {
  provider?: ProviderSpecView
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

function normalizeBaseUrl(value: string | undefined): string {
  return (value ?? '').trim().replace(/\/+$/, '').toLowerCase()
}

function profileMatchesProviderEndpoint(
  profile: ProfileView,
  provider: ProviderSpecView
): boolean {
  const profileBaseUrl = normalizeBaseUrl(profile.baseUrl)
  if (!profileBaseUrl) return false
  return provider.endpoints.some(
    (endpoint) => normalizeBaseUrl(endpoint.baseUrl) === profileBaseUrl
  )
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
    ) ??
    profiles.find(
      (profile) =>
        profile.wireFormat === provider.wireFormat &&
        profile.authScheme === provider.authScheme &&
        profileMatchesProviderEndpoint(profile, provider)
    )
  )
}

export default function SettingsPage({
  isSidebarOpen,
  onToggleSidebar,
  onOpenPlugins,
}: SettingsPageProps) {
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const [section, setSection] = useState<SettingsSection>('models')
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
  const [removingProfileName, setRemovingProfileName] = useState<string | null>(
    null
  )
  const [activatingProviderId, setActivatingProviderId] = useState<
    string | null
  >(null)
  const [activatingProfileName, setActivatingProfileName] = useState<
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
    const loadSettings = async () => {
      const [configResult, catalogResult] = await Promise.allSettled([
        api.getConfig(),
        api.getProviderCatalog(),
      ])
      if (cancelled) return

      const errors: string[] = []
      if (configResult.status === 'fulfilled') {
        applyConfig(configResult.value)
      } else {
        errors.push(`加载配置失败：${String(configResult.reason)}`)
      }

      if (catalogResult.status === 'fulfilled') {
        setProviderCatalog(catalogResult.value.providers)
      } else {
        errors.push(`加载 Provider 列表失败：${String(catalogResult.reason)}`)
      }

      setErrorMessage(errors.length > 0 ? errors.join('\n') : null)
      setLoading(false)
    }

    void loadSettings()
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

  const handleActivateConfiguredProfile = useCallback(
    async (profile: ProfileView) => {
      const modelId = profile.models[0]?.id
      if (!modelId) return

      setActivatingProfileName(profile.name)
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
        setActivatingProfileName(null)
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
      setRemovingProfileName(dialog.profile.name)
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
        setRemovingProfileName(null)
      }
    },
    [applyConfig, bumpModelRefreshKey]
  )

  const handleThemeChange = useCallback((preference: ThemePreference) => {
    setThemePreference(preference)
    setTheme(preference)
  }, [])

  const activeSectionMeta =
    SETTINGS_NAV_ITEMS.find((item) => item.id === section) ??
    SETTINGS_NAV_ITEMS[0]

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

      <main className="min-h-0 flex-1 overflow-y-auto px-[var(--layout-page-padding-x)] py-7">
        <div className="mx-auto grid w-full max-w-[1040px] gap-7 lg:grid-cols-[176px_minmax(0,1fr)]">
          <aside className="min-w-0">
            <div className="sticky top-0">
              <h1 className="px-2 text-[20px] font-semibold leading-tight text-text-primary">
                设置
              </h1>
              <nav className="mt-5 space-y-0.5">
                {SETTINGS_NAV_ITEMS.map((item) => (
                  <button
                    key={item.id}
                    type="button"
                    className="group block w-full text-left"
                    onClick={() => setSection(item.id)}
                  >
                    <span
                      className={cn(
                        'flex min-h-10 w-full items-center gap-2.5 rounded-md px-2.5 transition-colors',
                        section === item.id
                          ? 'text-text-primary'
                          : 'text-text-secondary group-hover:text-text-primary'
                      )}
                    >
                      <span
                        className="h-1.5 w-1.5 shrink-0 rounded-full"
                        style={{
                          backgroundColor:
                            section === item.id
                              ? 'var(--accent-strong)'
                              : 'transparent',
                        }}
                      />
                      <Icon
                        name={item.icon}
                        size={15}
                        className="shrink-0 text-current"
                      />
                      <span className="min-w-0 truncate text-[13px] font-medium">
                        {item.label}
                      </span>
                    </span>
                  </button>
                ))}
              </nav>
            </div>
          </aside>

          <section className="min-w-0">
            <div className="mb-4 flex min-w-0 items-start justify-between gap-4">
              <div className="min-w-0">
                <h2 className="flex items-center gap-2 text-[18px] font-semibold leading-tight text-text-primary">
                  <Icon
                    name={activeSectionMeta.icon}
                    size={17}
                    className="text-text-muted"
                  />
                  {activeSectionMeta.label}
                </h2>
                <p className="mt-1 text-[13px] text-text-muted">
                  {activeSectionMeta.hint}
                </p>
              </div>
              <div className="hidden min-w-0 max-w-[360px] rounded-md border border-border bg-surface-soft px-2.5 py-1.5 text-right font-mono text-[11px] text-text-muted lg:block">
                <span className="block truncate">
                  {configView?.configPath ?? ''}
                </span>
              </div>
            </div>

            {loading ? (
              <div className="flex items-center gap-2 text-[13px] text-text-secondary">
                <span className="h-4 w-4 animate-spin rounded-full border-2 border-border border-t-text-secondary" />
                加载设置...
              </div>
            ) : section === 'models' ? (
              <div className={cn(settingsPanelClass, settingsDividerClass)}>
                <div className="grid gap-0 divide-y divide-border md:grid-cols-2 md:divide-x md:divide-y-0">
                  <div className="min-w-0 px-4 py-3">
                    <div className="flex items-center justify-between gap-3">
                      <span className="text-[12px] font-medium text-text-muted">
                        主模型
                      </span>
                      <span className={compactPillClass}>
                        {currentProfile ? wireLabel(currentProfile) : '-'}
                      </span>
                    </div>
                    <div className="mt-2 truncate text-[15px] font-semibold text-text-primary">
                      {selectedModel || '-'}
                    </div>
                    <div className="mt-1 truncate text-[12px] text-text-secondary">
                      {selectedProfile || '-'}
                    </div>
                  </div>
                  <div className="min-w-0 px-4 py-3">
                    <div className="flex items-center justify-between gap-3">
                      <span className="text-[12px] font-medium text-text-muted">
                        小模型
                      </span>
                      <span className={compactPillClass}>
                        {currentSmallProfile
                          ? wireLabel(currentSmallProfile)
                          : '可选'}
                      </span>
                    </div>
                    <div className="mt-2 truncate text-[15px] font-semibold text-text-primary">
                      {selectedSmallModel || '未启用'}
                    </div>
                    <div className="mt-1 truncate text-[12px] text-text-secondary">
                      {selectedSmallProfile || '不使用'}
                    </div>
                  </div>
                </div>

                <div className={settingsRowClass}>
                  <div className="min-w-0">
                    <div className="text-[13px] font-medium text-text-primary">
                      Profile
                    </div>
                    <div className="mt-0.5 truncate text-[12px] text-text-muted">
                      {currentProfile
                        ? `${wireLabel(currentProfile)} · ${authLabel(currentProfile)}`
                        : '-'}
                    </div>
                  </div>
                  <select
                    className={cn(fieldInput, 'max-w-full sm:w-[320px]')}
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
                </div>
                <div className={settingsRowClass}>
                  <div className="min-w-0">
                    <div className="text-[13px] font-medium text-text-primary">
                      Model
                    </div>
                    <div className="mt-0.5 truncate text-[12px] text-text-muted">
                      当前对话默认模型
                    </div>
                  </div>
                  <select
                    className={cn(fieldInput, 'max-w-full sm:w-[320px]')}
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
                <div className={settingsRowClass}>
                  <div className="min-w-0">
                    <div className="text-[13px] font-medium text-text-primary">
                      Small Profile
                    </div>
                    <div className="mt-0.5 truncate text-[12px] text-text-muted">
                      轻量任务模型配置
                    </div>
                  </div>
                  <select
                    className={cn(fieldInput, 'max-w-full sm:w-[320px]')}
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
                </div>
                <div className={settingsRowClass}>
                  <div className="min-w-0">
                    <div className="text-[13px] font-medium text-text-primary">
                      Small Model
                    </div>
                    <div className="mt-0.5 truncate text-[12px] text-text-muted">
                      {currentSmallProfile
                        ? `${wireLabel(currentSmallProfile)} · ${authLabel(currentSmallProfile)}`
                        : '未启用'}
                    </div>
                  </div>
                  <select
                    className={cn(fieldInput, 'max-w-full sm:w-[320px]')}
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
                <div className={settingsRowClass}>
                  <span className="text-[13px] text-text-secondary">
                    Base URL
                  </span>
                  <span className="min-w-0 break-all text-left text-[13px] text-text-primary sm:text-right">
                    {currentProfile?.baseUrl ?? '-'}
                  </span>
                </div>
                <div className={settingsRowClass}>
                  <span className="text-[13px] text-text-secondary">
                    API Key
                  </span>
                  <span className="text-[13px] text-text-primary">
                    {currentProfile?.hasApiKey ? '已配置' : '未配置'}
                  </span>
                </div>
                <div className="flex flex-wrap justify-end gap-2.5 bg-panel-bg/35 px-4 py-3">
                  <Button
                    variant="secondary"
                    className={quietButtonClass}
                    onClick={() => void handleReload()}
                    disabled={reloading || saving || testing}
                  >
                    {reloading ? '重载中...' : '从磁盘重载'}
                  </Button>
                  <Button
                    variant="secondary"
                    className={quietButtonClass}
                    onClick={() => void handleTest()}
                    disabled={
                      testing || saving || !selectedProfile || !selectedModel
                    }
                  >
                    {testing ? '测试中...' : '测试连接'}
                  </Button>
                  <button
                    type="button"
                    className={settingsPrimaryButtonClass}
                    onClick={() => void handleSave()}
                    disabled={
                      saving || testing || !selectedProfile || !selectedModel
                    }
                  >
                    {saving ? '保存中...' : '保存模型'}
                  </button>
                </div>
              </div>
            ) : section === 'providers' ? (
              <div className="space-y-4">
                <div className={cn(settingsPanelClass, settingsDividerClass)}>
                  <div className="flex items-center justify-between gap-4 px-4 py-3">
                    <div className="min-w-0">
                      <h2 className="text-[13px] font-semibold text-text-primary">
                        已配置 Profiles
                      </h2>
                      <div className="mt-0.5 text-[12px] text-text-muted">
                        {profiles.length} configured
                      </div>
                    </div>
                    <Button
                      variant="secondary"
                      className={quietButtonClass}
                      onClick={() => void handleReload()}
                      disabled={reloading || applyingProviderId !== null}
                    >
                      {reloading ? '重载中...' : '重载'}
                    </Button>
                  </div>
                  {profiles.length === 0 ? (
                    <div className="px-4 py-6 text-center text-[13px] text-text-muted">
                      暂无配置
                    </div>
                  ) : (
                    profiles.map((profile) => {
                      const configuredModel =
                        profile.models.find(
                          (model) => model.id === selectedModel
                        )?.id ?? profile.models[0]?.id
                      const isActive = profile.name === selectedProfile
                      const isBusy = activatingProfileName === profile.name
                      const isRemoving = removingProfileName === profile.name
                      const canRemove =
                        !applyingProviderId &&
                        !removingProfileName &&
                        !activatingProviderId &&
                        !activatingProfileName &&
                        !saving &&
                        !reloading &&
                        !testing
                      const canActivate =
                        Boolean(configuredModel) &&
                        !isActive &&
                        !applyingProviderId &&
                        !removingProfileName &&
                        !activatingProviderId &&
                        !activatingProfileName &&
                        !saving &&
                        !reloading &&
                        !testing

                      return (
                        <div
                          key={profile.name}
                          className="grid min-w-0 gap-3 px-4 py-3 md:grid-cols-[minmax(180px,1.2fr)_minmax(180px,1fr)_auto] md:items-center"
                        >
                          <div className="min-w-0">
                            <div className="flex min-w-0 items-center gap-2">
                              <span className="truncate text-[13px] font-semibold text-text-primary">
                                {profile.name}
                              </span>
                              {isActive && (
                                <span className="shrink-0 rounded-md bg-success-soft px-2 py-0.5 text-[11px] font-medium text-success">
                                  当前
                                </span>
                              )}
                            </div>
                            <div className="mt-1 flex min-w-0 flex-wrap gap-1.5 text-[11px] text-text-muted">
                              <span>{profile.providerKind}</span>
                              <span>·</span>
                              <span>{wireFormatLabel(profile.wireFormat)}</span>
                              <span>·</span>
                              <span>{authSchemeLabel(profile.authScheme)}</span>
                            </div>
                          </div>
                          <div className="min-w-0 space-y-1 text-[12px]">
                            <div className="truncate text-text-primary">
                              {configuredModel ?? '-'}
                            </div>
                            <div className="truncate text-text-muted">
                              {profile.baseUrl || '-'}
                            </div>
                            <div className="text-text-muted">
                              Key {profile.hasApiKey ? '已配置' : '未配置'}
                            </div>
                          </div>
                          <div className="flex flex-wrap justify-start gap-2 md:justify-end">
                            {!isActive && (
                              <Button
                                variant="secondary"
                                className={quietButtonClass}
                                disabled={!canActivate}
                                onClick={() =>
                                  void handleActivateConfiguredProfile(profile)
                                }
                              >
                                {isBusy ? '切换中...' : '设为当前'}
                              </Button>
                            )}
                            <Button
                              variant="danger"
                              className={settingsDangerButtonClass}
                              disabled={!canRemove}
                              onClick={() => {
                                setProviderRemoveDialog({ profile })
                              }}
                            >
                              {isRemoving ? '移除中...' : '移除'}
                            </Button>
                          </div>
                        </div>
                      )
                    })
                  )}
                </div>

                {catalogProviders.length > 0 && (
                  <div className={cn(settingsPanelClass, settingsDividerClass)}>
                    <div className="flex items-center justify-between gap-4 px-4 py-3">
                      <div>
                        <h2 className="text-[13px] font-semibold text-text-primary">
                          Provider Presets
                        </h2>
                        <div className="mt-0.5 text-[12px] text-text-muted">
                          {catalogProviders.length} presets
                        </div>
                      </div>
                    </div>
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
                        removingProfileName === existingProviderProfile?.name ||
                        activatingProviderId === provider.id
                      const canConfigure =
                        !applyingProviderId &&
                        !removingProfileName &&
                        !activatingProviderId &&
                        !saving &&
                        !reloading &&
                        !testing
                      const canActivate =
                        Boolean(existingProviderProfile && configuredModel) &&
                        !isActive &&
                        canConfigure
                      const canRemove =
                        Boolean(existingProviderProfile) && canConfigure
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
                          className="grid min-w-0 gap-3 px-4 py-3 md:grid-cols-[minmax(180px,1.2fr)_minmax(180px,1fr)_auto] md:items-center"
                        >
                          <div className="min-w-0">
                            <div className="flex min-w-0 items-center gap-2">
                              <span className="truncate text-[13px] font-semibold text-text-primary">
                                {provider.displayName}
                              </span>
                              <span
                                className={cn(
                                  'shrink-0 rounded-md px-2 py-0.5 text-[11px] font-medium',
                                  isActive
                                    ? 'bg-success-soft text-success'
                                    : isConfigured
                                      ? 'bg-accent-soft text-accent'
                                      : 'bg-surface-muted text-text-muted'
                                )}
                              >
                                {isActive
                                  ? '当前'
                                  : isConfigured
                                    ? '已配置'
                                    : '未配置'}
                              </span>
                            </div>
                            <div className="mt-1 flex min-w-0 flex-wrap gap-1.5 text-[11px] text-text-muted">
                              <span>{provider.providerKind}</span>
                              <span>·</span>
                              <span>
                                {wireFormatLabel(provider.wireFormat)}
                              </span>
                              <span>·</span>
                              <span>
                                {authSchemeLabel(provider.authScheme)}
                              </span>
                            </div>
                            {capabilityLabels.length > 0 && (
                              <div className="mt-2 flex flex-wrap gap-1.5">
                                {capabilityLabels.map((label) => (
                                  <span
                                    key={label}
                                    className={compactPillClass}
                                  >
                                    {label}
                                  </span>
                                ))}
                              </div>
                            )}
                          </div>
                          <div className="min-w-0 space-y-1 text-[12px]">
                            <div className="truncate text-text-primary">
                              {configuredModel ?? provider.defaultModel}
                            </div>
                            <div className="truncate text-text-muted">
                              {displayedBaseUrl}
                            </div>
                            <div className="truncate text-text-muted">
                              {existingProviderProfile?.hasApiKey
                                ? 'Key 已配置'
                                : provider.apiKeyEnvVars[0]
                                  ? `Key env:${provider.apiKeyEnvVars[0]}`
                                  : 'Key 未配置'}
                            </div>
                          </div>
                          <div className="flex flex-wrap justify-start gap-2 md:justify-end">
                            {existingProviderProfile && !isActive && (
                              <Button
                                variant="secondary"
                                className={quietButtonClass}
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
                            {existingProviderProfile && (
                              <Button
                                variant="danger"
                                className={settingsDangerButtonClass}
                                disabled={!canRemove}
                                onClick={() => {
                                  setProviderRemoveDialog({
                                    provider,
                                    profile: existingProviderProfile,
                                  })
                                }}
                              >
                                移除
                              </Button>
                            )}
                            <Button
                              variant="secondary"
                              className={quietButtonClass}
                              disabled={!canConfigure || isProviderBusy}
                              onClick={() =>
                                openProviderConfigDialog(
                                  provider,
                                  existingProviderProfile
                                )
                              }
                            >
                              {isConfigured ? '编辑' : '配置'}
                            </Button>
                          </div>
                        </div>
                      )
                    })}
                  </div>
                )}
              </div>
            ) : section === 'permissions' ? (
              <div className="space-y-4">
                <div className={cn(settingsPanelClass, settingsDividerClass)}>
                  <label
                    className={cn(
                      settingsRowClass,
                      'cursor-pointer transition-colors hover:bg-surface-muted',
                      !yoloEnabled && 'bg-surface-muted/60'
                    )}
                  >
                    <span className="min-w-0">
                      <span className="block text-[13px] font-medium text-text-primary">
                        手动确认
                      </span>
                      <span className="mt-0.5 block text-[12px] text-text-muted">
                        工具调用前请求批准
                      </span>
                    </span>
                    <input
                      type="radio"
                      name="approvalMode"
                      checked={!yoloEnabled}
                      onChange={() => {
                        setYoloEnabled(false)
                        setStatusMessage(null)
                        setErrorMessage(null)
                      }}
                      className="h-4 w-4 shrink-0 accent-accent-strong"
                    />
                  </label>
                  <label
                    className={cn(
                      settingsRowClass,
                      'cursor-pointer transition-colors hover:bg-surface-muted',
                      yoloEnabled && 'bg-surface-muted/60'
                    )}
                  >
                    <span className="min-w-0">
                      <span className="block text-[13px] font-medium text-text-primary">
                        完全访问
                      </span>
                      <span className="mt-0.5 block text-[12px] text-text-muted">
                        自动批准工具调用
                      </span>
                    </span>
                    <input
                      type="radio"
                      name="approvalMode"
                      checked={yoloEnabled}
                      onChange={() => {
                        setYoloEnabled(true)
                        setStatusMessage(null)
                        setErrorMessage(null)
                      }}
                      className="h-4 w-4 shrink-0 accent-accent-strong"
                    />
                  </label>
                </div>

                <div className="flex flex-wrap justify-end gap-2.5">
                  <button
                    type="button"
                    className={settingsPrimaryButtonClass}
                    onClick={() => void handleSave()}
                    disabled={saving || testing}
                  >
                    {saving ? '保存中...' : '保存权限'}
                  </button>
                </div>
              </div>
            ) : (
              <div className={cn(settingsPanelClass, settingsDividerClass)}>
                {THEME_OPTIONS.map((option) => (
                  <label
                    key={option.value}
                    className="flex cursor-pointer items-center justify-between gap-4 px-4 py-3 transition-colors hover:bg-surface-muted"
                  >
                    <span className="min-w-0">
                      <span className="block text-[13px] font-medium text-text-primary">
                        {option.label}
                      </span>
                      <span className="mt-0.5 block text-[12px] text-text-muted">
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
                <div className="mt-2 text-[12px] text-text-muted">
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
          title={`移除 ${providerRemoveDialog.provider?.displayName ?? providerRemoveDialog.profile.name} 配置`}
          onClose={() => {
            if (removingProfileName) return
            setProviderRemoveDialog(null)
          }}
          className="w-[460px]"
        >
          <div className="space-y-4">
            <p className="text-[13px] leading-relaxed text-text-secondary">
              这会删除 {providerRemoveDialog.profile.name} 的 Base URL、API Key
              和模型配置。
            </p>
            {providerRemoveDialog.profile.name === selectedProfile && (
              <p className="rounded-lg border border-warning/20 bg-warning-soft px-3 py-2 text-[13px] text-warning">
                当前正在使用这个 Provider，移除后会切换到其他可用配置。
              </p>
            )}
            <div className="flex justify-end gap-2 pt-2">
              <Button
                variant="secondary"
                disabled={Boolean(removingProfileName)}
                onClick={() => setProviderRemoveDialog(null)}
              >
                返回
              </Button>
              <Button
                variant="danger"
                disabled={Boolean(removingProfileName)}
                onClick={() =>
                  void handleRemoveProviderPreset(providerRemoveDialog)
                }
              >
                {removingProfileName ? '移除中...' : '移除配置'}
              </Button>
            </div>
          </div>
        </Modal>
      )}
    </div>
  )
}
