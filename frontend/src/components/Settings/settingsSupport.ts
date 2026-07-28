import type { ThemePreference } from '../../lib/theme'
import type {
  ThinkingCapabilityDto,
  ThinkingConfigDto,
  UpdateModelOptionsRequest,
} from '../../services/generated'
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

// ── Thinking Form ──

export type ThinkingFormMode = 'default' | 'enabled' | 'disabled'

export interface ThinkingFormValue {
  mode: ThinkingFormMode
  effort: string
  budgetTokens: string
}

type ThinkingFormCapability = Pick<
  ThinkingCapabilityDto,
  'allowedEffort' | 'budgetMin' | 'budgetMax' | 'canDisable'
>

export const DEFAULT_THINKING_FORM: ThinkingFormValue = {
  mode: 'default',
  effort: '',
  budgetTokens: '',
}

export const EFFORT_LABELS: Record<string, string> = {
  low: '低',
  medium: '中',
  high: '高',
  minimal: '最小',
  max: '最大',
}

/**
 * 从模型的 thinkingCapability 和当前 thinking 配置推导表单初始值。
 * 模型无 thinkingCapability 时不参与 UI 渲染，此处仍返回 default。
 */
export function deriveThinkingFormValue(
  thinkingCapability: ThinkingFormCapability | null | undefined,
  currentThinking: ThinkingConfigDto | null | undefined
): ThinkingFormValue {
  if (!thinkingCapability) {
    return DEFAULT_THINKING_FORM
  }
  if (currentThinking == null) {
    return DEFAULT_THINKING_FORM
  }
  if (currentThinking.enabled) {
    return {
      mode: 'enabled',
      effort: currentThinking.effort ?? '',
      budgetTokens:
        currentThinking.budgetTokens != null
          ? String(currentThinking.budgetTokens)
          : '',
    }
  }
  return thinkingCapability.canDisable === false
    ? DEFAULT_THINKING_FORM
    : { mode: 'disabled', effort: '', budgetTokens: '' }
}

/**
 * 将表单值映射为 API 请求体。
 * default → thinking 为 undefined 以恢复默认；
 * disabled → { enabled: false }；
 * enabled → { enabled: true } 附带 effort/budgetTokens。
 */
export function thinkingFormToRequest(
  profileName: string,
  modelId: string,
  form: ThinkingFormValue,
  capability?: ThinkingFormCapability | null
): UpdateModelOptionsRequest {
  const base: UpdateModelOptionsRequest = { profileName, modelId }
  if (form.mode === 'default') {
    return { ...base, thinking: undefined }
  }
  if (!capability) {
    throw new Error('此模型未声明 Thinking 能力')
  }
  if (form.mode === 'disabled') {
    if (capability.canDisable === false) {
      throw new Error('此模型不支持关闭 Thinking')
    }
    return { ...base, thinking: { enabled: false } }
  }
  const allowedEffort = capability.allowedEffort
  if (Array.isArray(allowedEffort)) {
    if (allowedEffort.length > 0 && !form.effort) {
      throw new Error('请选择思考努力层级')
    }
    if (allowedEffort.length === 0 && form.effort) {
      throw new Error('此模型不支持设置思考努力层级')
    }
    if (
      form.effort &&
      allowedEffort.length > 0 &&
      !allowedEffort.includes(form.effort)
    ) {
      throw new Error(`不支持的思考努力层级：${form.effort}`)
    }
  }
  const supportsBudget =
    capability.budgetMin != null || capability.budgetMax != null
  const requiresBudget = capability.budgetMin != null
  if (requiresBudget && !form.budgetTokens) {
    throw new Error('请输入思考预算 Token')
  }
  if (!supportsBudget && form.budgetTokens) {
    throw new Error('此模型不支持设置思考预算 Token')
  }
  const thinking: ThinkingConfigDto = { enabled: true }
  if (form.effort) {
    thinking.effort = form.effort
  }
  if (form.budgetTokens) {
    const budgetTokens = Number(form.budgetTokens)
    if (!Number.isInteger(budgetTokens) || budgetTokens <= 0) {
      throw new Error('思考预算 Token 必须是正整数')
    }
    if (capability?.budgetMin != null && budgetTokens < capability.budgetMin) {
      throw new Error(`思考预算 Token 不能小于 ${capability.budgetMin}`)
    }
    if (capability?.budgetMax != null && budgetTokens > capability.budgetMax) {
      throw new Error(`思考预算 Token 不能大于 ${capability.budgetMax}`)
    }
    thinking.budgetTokens = budgetTokens
  }
  return { ...base, thinking }
}

/**
 * effort 选项列表（wire 值 ➝ 中文标签）。
 */
export function effortOptions(
  allowedEffort: string[]
): { label: string; value: string }[] {
  return allowedEffort.map((value) => ({
    label: EFFORT_LABELS[value] ?? value,
    value,
  }))
}

/**
 * 判断模型是否仅有 toggle（无 effort/budget 控制）。
 */
export function isToggleOnlyThinking(
  capability: ThinkingFormCapability | null | undefined
): boolean {
  if (!capability) return false
  const hasEffort =
    capability.allowedEffort == null || capability.allowedEffort.length > 0
  const hasBudget = capability.budgetMin != null || capability.budgetMax != null
  return !hasEffort && !hasBudget
}

// ── Model Selection ──

export interface ModelSelection {
  profileName: string
  modelId: string
  smallProfileName: string
  smallModelId: string
  thinkingFormValue: ThinkingFormValue
}

export const EMPTY_MODEL_SELECTION: ModelSelection = {
  profileName: '',
  modelId: '',
  smallProfileName: '',
  smallModelId: '',
  thinkingFormValue: DEFAULT_THINKING_FORM,
}

export function modelSelectionFromConfig(
  config: ConfigView,
  profiles: ProfileView[]
): ModelSelection {
  const profile = profiles.find((p) => p.name === config.activeProfile)
  const model = profile?.models.find((m) => m.id === config.activeModel)
  return {
    profileName: config.activeProfile,
    modelId: config.activeModel,
    smallProfileName: config.activeSmallProfile ?? '',
    smallModelId: config.activeSmallModel ?? '',
    thinkingFormValue: deriveThinkingFormValue(
      model?.thinkingCapability,
      model?.thinking
    ),
  }
}

export function deriveModelThinkingForm(
  profiles: ProfileView[],
  profileName: string,
  modelId: string
): ThinkingFormValue {
  const profile = profiles.find((p) => p.name === profileName)
  const model = profile?.models.find((m) => m.id === modelId)
  return deriveThinkingFormValue(model?.thinkingCapability, model?.thinking)
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
