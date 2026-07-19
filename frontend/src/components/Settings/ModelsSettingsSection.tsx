import type { ReactNode } from 'react'
import { fieldInput } from '../../lib/styles'
import { cn } from '../../lib/utils'
import type { ProfileView } from '../../services/types'
import { Button } from '../ui'
import {
  compactPillClass,
  type ModelSelection,
  type PendingOperation,
  pickModel,
  quietButtonClass,
  settingsDividerClass,
  settingsPanelClass,
  settingsPrimaryButtonClass,
  settingsRowClass,
  authLabel,
  wireLabel,
} from './settingsSupport'

interface ModelsSettingsSectionProps {
  profiles: ProfileView[]
  selection: ModelSelection
  operation: PendingOperation | null
  onSelectionChange: (patch: Partial<ModelSelection>) => void
  onReload: () => void
  onTest: () => void
  onSave: () => void
}

interface ModelSummaryProps {
  label: string
  badge: string
  model: string
  profile: string
}

function ModelSummary({ label, badge, model, profile }: ModelSummaryProps) {
  return (
    <div className="min-w-0 px-4 py-3">
      <div className="flex items-center justify-between gap-3">
        <span className="text-[12px] font-medium text-text-muted">{label}</span>
        <span className={compactPillClass}>{badge}</span>
      </div>
      <div className="mt-2 truncate text-[15px] font-semibold text-text-primary">
        {model}
      </div>
      <div className="mt-1 truncate text-[12px] text-text-secondary">
        {profile}
      </div>
    </div>
  )
}

interface SettingsSelectRowProps {
  label: string
  hint: string
  value: string
  disabled?: boolean
  children: ReactNode
  onChange: (value: string) => void
}

function SettingsSelectRow({
  label,
  hint,
  value,
  disabled,
  children,
  onChange,
}: SettingsSelectRowProps) {
  return (
    <div className={settingsRowClass}>
      <div className="min-w-0">
        <div className="text-[13px] font-medium text-text-primary">{label}</div>
        <div className="mt-0.5 truncate text-[12px] text-text-muted">
          {hint}
        </div>
      </div>
      <select
        className={cn(fieldInput, 'max-w-full sm:w-[320px]')}
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      >
        {children}
      </select>
    </div>
  )
}

export function ModelsSettingsSection({
  profiles,
  selection,
  operation,
  onSelectionChange,
  onReload,
  onTest,
  onSave,
}: ModelsSettingsSectionProps) {
  const currentProfile = profiles.find(
    (profile) => profile.name === selection.profileName
  )
  const currentSmallProfile = profiles.find(
    (profile) => profile.name === selection.smallProfileName
  )
  const busy = operation !== null
  const saving = operation?.kind === 'save'
  const reloading = operation?.kind === 'reload'
  const testing = operation?.kind === 'test'
  const hasPrimarySelection = Boolean(
    selection.profileName && selection.modelId
  )

  return (
    <div className={cn(settingsPanelClass, settingsDividerClass)}>
      <div className="grid gap-0 divide-y divide-border md:grid-cols-2 md:divide-x md:divide-y-0">
        <ModelSummary
          label="主模型"
          badge={currentProfile ? wireLabel(currentProfile) : '-'}
          model={selection.modelId || '-'}
          profile={selection.profileName || '-'}
        />
        <ModelSummary
          label="小模型"
          badge={currentSmallProfile ? wireLabel(currentSmallProfile) : '可选'}
          model={selection.smallModelId || '未启用'}
          profile={selection.smallProfileName || '不使用'}
        />
      </div>

      <SettingsSelectRow
        label="Profile"
        hint={
          currentProfile
            ? `${wireLabel(currentProfile)} · ${authLabel(currentProfile)}`
            : '-'
        }
        value={selection.profileName}
        onChange={(profileName) => {
          const profile = profiles.find((item) => item.name === profileName)
          onSelectionChange({
            profileName,
            modelId: pickModel(profile, selection.modelId),
          })
        }}
      >
        {profiles.map((profile) => (
          <option key={profile.name} value={profile.name}>
            {profile.name} · {wireLabel(profile)}
          </option>
        ))}
      </SettingsSelectRow>

      <SettingsSelectRow
        label="Model"
        hint="当前对话默认模型"
        value={selection.modelId}
        disabled={!currentProfile?.models.length}
        onChange={(modelId) => onSelectionChange({ modelId })}
      >
        {currentProfile?.models.map((model) => (
          <option key={model.id} value={model.id}>
            {model.id}
          </option>
        ))}
      </SettingsSelectRow>

      <SettingsSelectRow
        label="Small Profile"
        hint="轻量任务模型配置"
        value={selection.smallProfileName}
        onChange={(smallProfileName) => {
          const profile = profiles.find(
            (item) => item.name === smallProfileName
          )
          onSelectionChange({
            smallProfileName,
            smallModelId: smallProfileName
              ? (profile?.models[0]?.id ?? '')
              : '',
          })
        }}
      >
        <option value="">不使用</option>
        {profiles.map((profile) => (
          <option key={profile.name} value={profile.name}>
            {profile.name} · {wireLabel(profile)}
          </option>
        ))}
      </SettingsSelectRow>

      <SettingsSelectRow
        label="Small Model"
        hint={
          currentSmallProfile
            ? `${wireLabel(currentSmallProfile)} · ${authLabel(currentSmallProfile)}`
            : '未启用'
        }
        value={selection.smallModelId}
        disabled={!currentSmallProfile?.models.length}
        onChange={(smallModelId) => onSelectionChange({ smallModelId })}
      >
        {!currentSmallProfile && <option value="">不使用</option>}
        {currentSmallProfile?.models.map((model) => (
          <option key={model.id} value={model.id}>
            {model.id}
          </option>
        ))}
      </SettingsSelectRow>

      <div className={settingsRowClass}>
        <span className="text-[13px] text-text-secondary">Base URL</span>
        <span className="min-w-0 break-all text-left text-[13px] text-text-primary sm:text-right">
          {currentProfile?.baseUrl ?? '-'}
        </span>
      </div>
      <div className={settingsRowClass}>
        <span className="text-[13px] text-text-secondary">API Key</span>
        <span className="text-[13px] text-text-primary">
          {currentProfile?.hasApiKey ? '已配置' : '未配置'}
        </span>
      </div>
      <div className="flex flex-wrap justify-end gap-2.5 bg-panel-bg/35 px-4 py-3">
        <Button
          variant="secondary"
          className={quietButtonClass}
          onClick={onReload}
          disabled={busy}
        >
          {reloading ? '重载中...' : '从磁盘重载'}
        </Button>
        <Button
          variant="secondary"
          className={quietButtonClass}
          onClick={onTest}
          disabled={busy || !hasPrimarySelection}
        >
          {testing ? '测试中...' : '测试连接'}
        </Button>
        <button
          type="button"
          className={settingsPrimaryButtonClass}
          onClick={onSave}
          disabled={busy || !hasPrimarySelection}
        >
          {saving ? '保存中...' : '保存模型'}
        </button>
      </div>
    </div>
  )
}
