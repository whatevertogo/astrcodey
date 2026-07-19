import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { getStoredTheme, setTheme, type ThemePreference } from '../../lib/theme'
import { cn } from '../../lib/utils'
import * as api from '../../services/api'
import type {
  ConfigView,
  ProfileView,
  ProviderSpecView,
} from '../../services/types'
import { useAppStore } from '../../store/conversation'
import { PageHeader } from '../layout'
import { Button, Icon, IconButton } from '../ui'
import { ModelsSettingsSection } from './ModelsSettingsSection'
import { ProviderDialogs } from './ProviderDialogs'
import { ProvidersSettingsSection } from './ProvidersSettingsSection'
import {
  AppearanceSettingsSection,
  PermissionsSettingsSection,
  SettingsFeedbackView,
} from './SettingsPanels'
import {
  EMPTY_MODEL_SELECTION,
  modelSelectionFromConfig,
  type ModelSelection,
  type PendingOperation,
  type ProviderConfigDialogState,
  type ProviderDialogState,
  type ProviderRemoveDialogState,
  SETTINGS_NAV_ITEMS,
  type SettingsFeedback,
  type SettingsSection,
} from './settingsSupport'

const EMPTY_PROFILES: ProfileView[] = []

interface SettingsPageProps {
  isSidebarOpen: boolean
  onToggleSidebar: () => void
  onOpenPlugins: () => void
}

export default function SettingsPage({
  isSidebarOpen,
  onToggleSidebar,
  onOpenPlugins,
}: SettingsPageProps) {
  const bumpModelRefreshKey = useAppStore((state) => state.bumpModelRefreshKey)
  const [section, setSection] = useState<SettingsSection>('models')
  const [configView, setConfigView] = useState<ConfigView | null>(null)
  const [providerCatalog, setProviderCatalog] = useState<ProviderSpecView[]>([])
  const [selection, setSelection] = useState<ModelSelection>(
    EMPTY_MODEL_SELECTION
  )
  const [yoloEnabled, setYoloEnabled] = useState(false)
  const [themePreference, setThemePreference] =
    useState<ThemePreference>(getStoredTheme)
  const [loading, setLoading] = useState(true)
  const [operation, setOperation] = useState<PendingOperation | null>(null)
  const [feedback, setFeedback] = useState<SettingsFeedback | null>(null)
  const [providerDialog, setProviderDialog] =
    useState<ProviderDialogState | null>(null)

  const applyConfig = useCallback((config: ConfigView) => {
    setConfigView(config)
    setSelection(modelSelectionFromConfig(config))
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
      setFeedback(
        errors.length > 0 ? { kind: 'error', message: errors.join('\n') } : null
      )
      setLoading(false)
    }

    void loadSettings()
    return () => {
      cancelled = true
    }
  }, [applyConfig])

  const refreshConfig = useCallback(async () => {
    applyConfig(await api.getConfig())
  }, [applyConfig])

  const runOperation = useCallback(
    async (
      nextOperation: PendingOperation,
      task: () => Promise<void>,
      errorFeedback?: (error: unknown) => SettingsFeedback
    ) => {
      setOperation(nextOperation)
      setFeedback(null)
      try {
        await task()
      } catch (error) {
        setFeedback(
          errorFeedback?.(error) ?? {
            kind: 'error',
            message: String(error),
          }
        )
      } finally {
        setOperation(null)
      }
    },
    []
  )

  const handleSave = useCallback(() => {
    if (!selection.profileName || !selection.modelId) return
    return runOperation({ kind: 'save' }, async () => {
      await api.updateActiveSelection(
        selection.profileName,
        selection.modelId,
        selection.smallProfileName || undefined,
        selection.smallModelId || undefined,
        yoloEnabled ? 'yolo' : 'manual'
      )
      await refreshConfig()
      bumpModelRefreshKey()
      setFeedback({ kind: 'success', message: '已保存' })
    })
  }, [bumpModelRefreshKey, refreshConfig, runOperation, selection, yoloEnabled])

  const handleReload = useCallback(
    () =>
      runOperation({ kind: 'reload' }, async () => {
        await api.reloadConfig()
        await refreshConfig()
        setFeedback({ kind: 'success', message: '已从磁盘重载' })
      }),
    [refreshConfig, runOperation]
  )

  const handleTest = useCallback(
    () =>
      runOperation(
        { kind: 'test' },
        async () => {
          setFeedback({ kind: 'test', result: await api.testModel() })
        },
        (error) => ({
          kind: 'test',
          result: { success: false, message: String(error) },
        })
      ),
    [runOperation]
  )

  const openProviderConfigDialog = useCallback(
    (provider: ProviderSpecView, existingProfile?: ProfileView) => {
      const defaultEndpoint = provider.endpoints.find(
        (endpoint) => endpoint.isDefault
      )
      const modelId =
        existingProfile?.models.find((model) => model.id === selection.modelId)
          ?.id ??
        existingProfile?.models[0]?.id ??
        provider.defaultModel

      setProviderDialog({
        kind: 'config',
        value: {
          provider,
          existingProfile,
          baseUrl: existingProfile?.baseUrl ?? defaultEndpoint?.baseUrl ?? '',
          apiKey: '',
          modelId,
        },
      })
      setFeedback(null)
    },
    [selection.modelId]
  )

  const handleApplyProvider = useCallback(
    async (dialog: ProviderConfigDialogState, activate: boolean) => {
      const baseUrl = dialog.baseUrl.trim()
      if (!baseUrl) return

      await runOperation(
        { kind: 'apply-provider', providerId: dialog.provider.id },
        async () => {
          const response = await api.applyProviderPreset({
            providerId: dialog.provider.id,
            baseUrl,
            apiKey: dialog.apiKey.trim() || undefined,
            modelId: dialog.modelId.trim() || dialog.provider.defaultModel,
            activate,
          })
          await refreshConfig()
          if (activate) bumpModelRefreshKey()
          setProviderDialog(null)
          setFeedback({
            kind: 'success',
            message: response.warning
              ? `已保存 ${response.profileName}；${response.warning}`
              : response.activated
                ? `已应用 ${response.profileName}`
                : `已保存 ${response.profileName}`,
          })
        }
      )
    },
    [bumpModelRefreshKey, refreshConfig, runOperation]
  )

  const handleActivateProfile = useCallback(
    async (profile: ProfileView) => {
      const modelId = profile.models[0]?.id
      if (!modelId) return

      await runOperation(
        { kind: 'activate-profile', profileName: profile.name },
        async () => {
          const response = await api.updateActiveSelection(
            profile.name,
            modelId,
            selection.smallProfileName || undefined,
            selection.smallModelId || undefined,
            yoloEnabled ? 'yolo' : 'manual'
          )
          await refreshConfig()
          bumpModelRefreshKey()
          setFeedback({
            kind: 'success',
            message: response.warning
              ? `已切换到 ${profile.name}；${response.warning}`
              : `已切换到 ${profile.name}`,
          })
        }
      )
    },
    [bumpModelRefreshKey, refreshConfig, runOperation, selection, yoloEnabled]
  )

  const handleRemoveProvider = useCallback(
    async (dialog: ProviderRemoveDialogState) => {
      await runOperation(
        { kind: 'remove-profile', profileName: dialog.profile.name },
        async () => {
          const response = await api.removeProviderPreset(dialog.profile.name)
          await refreshConfig()
          bumpModelRefreshKey()
          setProviderDialog(null)
          setFeedback({
            kind: 'success',
            message: response.warning
              ? `已取消 ${response.removedProfileName}；${response.warning}`
              : `已取消 ${response.removedProfileName}`,
          })
        }
      )
    },
    [bumpModelRefreshKey, refreshConfig, runOperation]
  )

  const handleSelectionChange = useCallback(
    (patch: Partial<ModelSelection>) => {
      setSelection((current) => ({ ...current, ...patch }))
      setFeedback(null)
    },
    []
  )

  const handleProviderConfigChange = useCallback(
    (patch: Partial<ProviderConfigDialogState>) => {
      setProviderDialog((current) =>
        current?.kind === 'config'
          ? { kind: 'config', value: { ...current.value, ...patch } }
          : current
      )
      setFeedback(null)
    },
    []
  )

  const handleThemeChange = useCallback((preference: ThemePreference) => {
    setThemePreference(preference)
    setTheme(preference)
  }, [])

  const profiles = configView?.profiles ?? EMPTY_PROFILES
  const activeSectionMeta =
    SETTINGS_NAV_ITEMS.find((item) => item.id === section) ??
    SETTINGS_NAV_ITEMS[0]
  let sectionContent: ReactNode
  if (loading) {
    sectionContent = (
      <div className="flex items-center gap-2 text-[13px] text-text-secondary">
        <span className="h-4 w-4 animate-spin rounded-full border-2 border-border border-t-text-secondary" />
        加载设置...
      </div>
    )
  } else if (section === 'models') {
    sectionContent = (
      <ModelsSettingsSection
        profiles={profiles}
        selection={selection}
        operation={operation}
        onSelectionChange={handleSelectionChange}
        onReload={() => void handleReload()}
        onTest={() => void handleTest()}
        onSave={() => void handleSave()}
      />
    )
  } else if (section === 'providers') {
    sectionContent = (
      <ProvidersSettingsSection
        profiles={profiles}
        providers={providerCatalog}
        activeProfileName={selection.profileName}
        selectedModelId={selection.modelId}
        operation={operation}
        onReload={() => void handleReload()}
        onActivate={(profile) => void handleActivateProfile(profile)}
        onConfigure={openProviderConfigDialog}
        onRemove={(value) => {
          setProviderDialog({ kind: 'remove', value })
          setFeedback(null)
        }}
      />
    )
  } else if (section === 'permissions') {
    sectionContent = (
      <PermissionsSettingsSection
        yoloEnabled={yoloEnabled}
        operation={operation}
        onChange={(enabled) => {
          setYoloEnabled(enabled)
          setFeedback(null)
        }}
        onSave={() => void handleSave()}
      />
    )
  } else {
    sectionContent = (
      <AppearanceSettingsSection
        preference={themePreference}
        onChange={handleThemeChange}
      />
    )
  }

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
          <SettingsNavigation section={section} onChange={setSection} />
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
            {sectionContent}
            <SettingsFeedbackView feedback={feedback} />
          </section>
        </div>
      </main>

      <ProviderDialogs
        dialog={providerDialog}
        operation={operation}
        activeProfileName={selection.profileName}
        onConfigChange={handleProviderConfigChange}
        onClose={() => setProviderDialog(null)}
        onApply={(dialog, activate) =>
          void handleApplyProvider(dialog, activate)
        }
        onRemove={(dialog) => void handleRemoveProvider(dialog)}
      />
    </div>
  )
}

function SettingsNavigation({
  section,
  onChange,
}: {
  section: SettingsSection
  onChange: (section: SettingsSection) => void
}) {
  return (
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
              onClick={() => onChange(item.id)}
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
  )
}
