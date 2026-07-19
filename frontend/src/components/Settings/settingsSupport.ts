import type { ThemePreference } from '../../lib/theme'
import type {
  ConfigView,
  ModelTestResult,
  ProfileView,
  ProviderSpecView,
} from '../../services/types'
import {
  providerAuthSchemeLabel,
  providerWireFormatLabel,
} from '../../lib/providerLabels'
import type { IconName } from '../ui'

export type SettingsSection =
  | 'models'
  | 'providers'
  | 'permissions'
  | 'appearance'

export const SETTINGS_NAV_ITEMS: {
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

export const settingsPanelClass =
  'overflow-hidden rounded-lg border border-border bg-surface-soft'
export const settingsRowClass =
  'flex min-w-0 flex-col items-stretch justify-between gap-3 px-4 py-3 sm:flex-row sm:items-center sm:gap-4'
export const settingsDividerClass = 'divide-y divide-border'
export const quietButtonClass =
  'inline-flex h-8 items-center justify-center rounded-md border border-border-strong bg-surface px-3 text-[12px] font-medium text-text-primary transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-40'
export const settingsPrimaryButtonClass =
  'inline-flex h-8 items-center justify-center rounded-md border border-border-strong bg-btn-primary-bg px-3 text-[12px] font-medium text-btn-primary-fg transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40'
export const settingsDangerButtonClass =
  'inline-flex h-8 items-center justify-center rounded-md border border-danger/30 bg-danger-soft px-3 text-[12px] font-medium text-danger transition-colors hover:brightness-110 disabled:cursor-not-allowed disabled:opacity-40'
export const compactPillClass =
  'inline-flex min-h-6 shrink-0 items-center rounded-md border border-border bg-panel-bg px-2 text-[11px] font-medium text-text-secondary'

export interface ModelSelection {
  profileName: string
  modelId: string
  smallProfileName: string
  smallModelId: string
}

export const EMPTY_MODEL_SELECTION: ModelSelection = {
  profileName: '',
  modelId: '',
  smallProfileName: '',
  smallModelId: '',
}

export function modelSelectionFromConfig(config: ConfigView): ModelSelection {
  return {
    profileName: config.activeProfile,
    modelId: config.activeModel,
    smallProfileName: config.activeSmallProfile ?? '',
    smallModelId: config.activeSmallModel ?? '',
  }
}

export type PendingOperation =
  | { kind: 'save' }
  | { kind: 'reload' }
  | { kind: 'test' }
  | { kind: 'apply-provider'; providerId: string }
  | { kind: 'remove-profile'; profileName: string }
  | { kind: 'activate-profile'; profileName: string }

export type SettingsFeedback =
  | { kind: 'test'; result: ModelTestResult }
  | { kind: 'success'; message: string }
  | { kind: 'error'; message: string }

export interface ProviderConfigDialogState {
  provider: ProviderSpecView
  existingProfile?: ProfileView
  baseUrl: string
  apiKey: string
  modelId: string
}

export interface ProviderRemoveDialogState {
  provider?: ProviderSpecView
  profile: ProfileView
}

export type ProviderDialogState =
  | { kind: 'config'; value: ProviderConfigDialogState }
  | { kind: 'remove'; value: ProviderRemoveDialogState }

export const THEME_OPTIONS: {
  value: ThemePreference
  label: string
  hint: string
}[] = [
  { value: 'dark', label: '深色', hint: '适合长时间编码和低光环境' },
  { value: 'light', label: '浅色', hint: '适合明亮环境和文档阅读' },
  { value: 'system', label: '跟随系统', hint: '自动匹配系统外观设置' },
]

export function pickModel(
  profile: ProfileView | undefined,
  currentModel: string
): string {
  if (!profile || profile.models.length === 0) return ''
  return profile.models.some((model) => model.id === currentModel)
    ? currentModel
    : (profile.models[0]?.id ?? '')
}

export function wireLabel(profile: ProfileView | undefined): string {
  return profile ? providerWireFormatLabel(profile.wireFormat) : ''
}

export function authLabel(profile: ProfileView | undefined): string {
  return profile ? providerAuthSchemeLabel(profile.authScheme) : ''
}

function normalizeBaseUrl(value: string | null | undefined): string {
  return (value ?? '').trim().replace(/\/+$/, '').toLowerCase()
}

function profileMatchesProviderEndpoint(
  profile: ProfileView,
  provider: ProviderSpecView
): boolean {
  const profileBaseUrl = normalizeBaseUrl(profile.baseUrl)
  return (
    profileBaseUrl.length > 0 &&
    provider.endpoints.some(
      (endpoint) => normalizeBaseUrl(endpoint.baseUrl) === profileBaseUrl
    )
  )
}

export function findProviderProfile(
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
