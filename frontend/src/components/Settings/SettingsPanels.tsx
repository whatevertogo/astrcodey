import type { ThemePreference } from '../../lib/theme'
import { cn } from '../../lib/utils'
import {
  type PendingOperation,
  type SettingsFeedback,
  settingsDividerClass,
  settingsPanelClass,
  settingsPrimaryButtonClass,
  settingsRowClass,
  THEME_OPTIONS,
} from './settingsSupport'

interface PermissionsSettingsSectionProps {
  yoloEnabled: boolean
  operation: PendingOperation | null
  onChange: (enabled: boolean) => void
  onSave: () => void
}

export function PermissionsSettingsSection({
  yoloEnabled,
  operation,
  onChange,
  onSave,
}: PermissionsSettingsSectionProps) {
  const saving = operation?.kind === 'save'

  return (
    <div className="space-y-4">
      <div className={cn(settingsPanelClass, settingsDividerClass)}>
        <ApprovalModeOption
          title="手动确认"
          hint="工具调用前请求批准"
          checked={!yoloEnabled}
          onSelect={() => onChange(false)}
        />
        <ApprovalModeOption
          title="完全访问"
          hint="自动批准工具调用"
          checked={yoloEnabled}
          onSelect={() => onChange(true)}
        />
      </div>
      <div className="flex flex-wrap justify-end gap-2.5">
        <button
          type="button"
          className={settingsPrimaryButtonClass}
          onClick={onSave}
          disabled={operation !== null}
        >
          {saving ? '保存中...' : '保存权限'}
        </button>
      </div>
    </div>
  )
}

interface ApprovalModeOptionProps {
  title: string
  hint: string
  checked: boolean
  onSelect: () => void
}

function ApprovalModeOption({
  title,
  hint,
  checked,
  onSelect,
}: ApprovalModeOptionProps) {
  return (
    <label
      className={cn(
        settingsRowClass,
        'cursor-pointer transition-colors hover:bg-surface-muted',
        checked && 'bg-surface-muted/60'
      )}
    >
      <span className="min-w-0">
        <span className="block text-[13px] font-medium text-text-primary">
          {title}
        </span>
        <span className="mt-0.5 block text-[12px] text-text-muted">{hint}</span>
      </span>
      <input
        type="radio"
        name="approvalMode"
        checked={checked}
        onChange={onSelect}
        className="h-4 w-4 shrink-0 accent-accent-strong"
      />
    </label>
  )
}

interface AppearanceSettingsSectionProps {
  preference: ThemePreference
  onChange: (preference: ThemePreference) => void
}

export function AppearanceSettingsSection({
  preference,
  onChange,
}: AppearanceSettingsSectionProps) {
  return (
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
            checked={preference === option.value}
            onChange={() => onChange(option.value)}
            className="h-4 w-4 shrink-0 accent-accent-strong"
          />
        </label>
      ))}
    </div>
  )
}

export function SettingsFeedbackView({
  feedback,
}: {
  feedback: SettingsFeedback | null
}) {
  if (!feedback) return null
  if (feedback.kind === 'test') {
    return (
      <div
        className={cn(
          'mt-4 rounded-lg border px-4 py-3 text-[13px]',
          feedback.result.success
            ? 'border-success/20 bg-success-soft text-success'
            : 'border-danger/20 bg-danger-soft text-danger'
        )}
      >
        {feedback.result.success ? '连接成功' : '连接失败'}:{' '}
        {feedback.result.message}
      </div>
    )
  }
  return (
    <div
      className={cn(
        'mt-4 rounded-lg border px-4 py-3 text-[13px]',
        feedback.kind === 'success'
          ? 'border-success/20 bg-success-soft text-success'
          : 'border-danger/20 bg-danger-soft text-danger'
      )}
    >
      {feedback.message}
    </div>
  )
}
