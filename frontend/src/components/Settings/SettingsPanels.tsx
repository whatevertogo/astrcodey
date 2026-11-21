import { fieldInput } from '../../lib/styles'
import type { ThemePreference } from '../../lib/theme'
import { cn } from '../../lib/utils'
import type { WebSearchConfigView } from '../../services/types'
import { Button } from '../ui'
import {
  compactPillClass,
  type PendingOperation,
  quietButtonClass,
  type SettingsFeedback,
  settingsDangerButtonClass,
  settingsDividerClass,
  settingsPanelClass,
  settingsPrimaryButtonClass,
  settingsRowClass,
  THEME_OPTIONS,
  WEB_SEARCH_PROVIDER_OPTIONS,
  type WebSearchFormValue,
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

interface WebSearchSettingsSectionProps {
  webSearchView: WebSearchConfigView | null
  form: WebSearchFormValue
  operation: PendingOperation | null
  onChange: (patch: Partial<WebSearchFormValue>) => void
  onReload: () => void
  onSave: () => void
}

export function WebSearchSettingsSection({
  webSearchView,
  form,
  operation,
  onChange,
  onReload,
  onSave,
}: WebSearchSettingsSectionProps) {
  const busy = operation !== null
  const saving = operation?.kind === 'save-web-search'
  const reloading = operation?.kind === 'reload'

  const hasDeepseekKey =
    Boolean(webSearchView?.hasDeepseekApiKey) && !form.clearDeepseekApiKey
  const hasBraveKey =
    Boolean(webSearchView?.hasBraveApiKey) && !form.clearBraveApiKey
  const hasSerperKey =
    Boolean(webSearchView?.hasSerperApiKey) && !form.clearSerperApiKey

  return (
    <div className="space-y-4">
      <div className={cn(settingsPanelClass, settingsDividerClass)}>
        {WEB_SEARCH_PROVIDER_OPTIONS.map((option) => {
          const checked = form.provider === option.value
          return (
            <label
              key={option.value}
              className={cn(
                settingsRowClass,
                'cursor-pointer transition-colors hover:bg-surface-muted',
                checked && 'bg-surface-muted/60'
              )}
            >
              <span className="min-w-0">
                <span className="flex flex-wrap items-center gap-2">
                  <span className="text-[13px] font-medium text-text-primary">
                    {option.label}
                  </span>
                  <span className={compactPillClass}>{option.badge}</span>
                </span>
                <span className="mt-1 block text-[12px] leading-relaxed text-text-muted">
                  {option.hint}
                </span>
              </span>
              <input
                type="radio"
                name="webSearchProvider"
                value={option.value}
                checked={checked}
                disabled={busy}
                onChange={() => onChange({ provider: option.value })}
                className="h-4 w-4 shrink-0 accent-accent-strong"
              />
            </label>
          )
        })}
      </div>

      {form.provider === 'deepseek' && (
        <div className={cn(settingsPanelClass, settingsDividerClass)}>
          <WebSearchApiKeyRows
            providerLabel="DeepSeek"
            hasSavedKey={hasDeepseekKey}
            cleared={form.clearDeepseekApiKey}
            apiKey={form.deepseekApiKey}
            apiKeyEnv={form.deepseekApiKeyEnv}
            defaultEnvPlaceholder="DEEPSEEK_API_KEY（默认）"
            disabled={busy}
            onApiKeyChange={(deepseekApiKey) =>
              onChange({
                deepseekApiKey,
                clearDeepseekApiKey: false,
              })
            }
            onClearSavedKey={() =>
              onChange({
                deepseekApiKey: '',
                clearDeepseekApiKey: true,
              })
            }
            onApiKeyEnvChange={(deepseekApiKeyEnv) =>
              onChange({ deepseekApiKeyEnv })
            }
          />
        </div>
      )}

      {form.provider === 'brave' && (
        <div className={cn(settingsPanelClass, settingsDividerClass)}>
          <WebSearchApiKeyRows
            providerLabel="Brave Search"
            hasSavedKey={hasBraveKey}
            cleared={form.clearBraveApiKey}
            apiKey={form.braveApiKey}
            apiKeyEnv={form.braveApiKeyEnv}
            defaultEnvPlaceholder="可选，例如 BRAVE_API_KEY"
            disabled={busy}
            onApiKeyChange={(braveApiKey) =>
              onChange({
                braveApiKey,
                clearBraveApiKey: false,
              })
            }
            onClearSavedKey={() =>
              onChange({
                braveApiKey: '',
                clearBraveApiKey: true,
              })
            }
            onApiKeyEnvChange={(braveApiKeyEnv) => onChange({ braveApiKeyEnv })}
          />
        </div>
      )}

      {form.provider === 'serper' && (
        <div className={cn(settingsPanelClass, settingsDividerClass)}>
          <WebSearchApiKeyRows
            providerLabel="Serper"
            hasSavedKey={hasSerperKey}
            cleared={form.clearSerperApiKey}
            apiKey={form.serperApiKey}
            apiKeyEnv={form.serperApiKeyEnv}
            defaultEnvPlaceholder="可选，例如 SERPER_API_KEY"
            disabled={busy}
            onApiKeyChange={(serperApiKey) =>
              onChange({
                serperApiKey,
                clearSerperApiKey: false,
              })
            }
            onClearSavedKey={() =>
              onChange({
                serperApiKey: '',
                clearSerperApiKey: true,
              })
            }
            onApiKeyEnvChange={(serperApiKeyEnv) =>
              onChange({ serperApiKeyEnv })
            }
          />
        </div>
      )}

      <div className={cn(settingsPanelClass, settingsDividerClass)}>
        <div className={settingsRowClass}>
          <div className="min-w-0">
            <div className="text-[13px] font-medium text-text-primary">
              默认返回条数
            </div>
            <div className="mt-0.5 text-[12px] text-text-muted">
              单次搜索未显式指定 maxResults 时的默认结果数量（1–20）
            </div>
          </div>
          <input
            type="number"
            min={1}
            max={20}
            className={cn(fieldInput, 'max-w-full sm:w-[160px]')}
            value={form.defaultMaxResults}
            disabled={busy}
            onChange={(event) =>
              onChange({ defaultMaxResults: event.target.value })
            }
          />
        </div>

        <div className={settingsRowClass}>
          <div className="min-w-0">
            <div className="text-[13px] font-medium text-text-primary">
              请求超时（秒）
            </div>
            <div className="mt-0.5 text-[12px] text-text-muted">
              网页搜索请求的超时时间
            </div>
          </div>
          <input
            type="number"
            min={1}
            className={cn(fieldInput, 'max-w-full sm:w-[160px]')}
            value={form.requestTimeoutSecs}
            disabled={busy}
            onChange={(event) =>
              onChange({ requestTimeoutSecs: event.target.value })
            }
          />
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
          <button
            type="button"
            className={settingsPrimaryButtonClass}
            onClick={onSave}
            disabled={busy}
          >
            {saving ? '保存中...' : '保存搜索设置'}
          </button>
        </div>
      </div>
    </div>
  )
}

interface WebSearchApiKeyRowsProps {
  providerLabel: string
  hasSavedKey: boolean
  cleared: boolean
  apiKey: string
  apiKeyEnv: string
  defaultEnvPlaceholder: string
  disabled: boolean
  onApiKeyChange: (value: string) => void
  onClearSavedKey: () => void
  onApiKeyEnvChange: (value: string) => void
}

function WebSearchApiKeyRows({
  providerLabel,
  hasSavedKey,
  cleared,
  apiKey,
  apiKeyEnv,
  defaultEnvPlaceholder,
  disabled,
  onApiKeyChange,
  onClearSavedKey,
  onApiKeyEnvChange,
}: WebSearchApiKeyRowsProps) {
  return (
    <>
      <div className={settingsRowClass}>
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="text-[13px] font-medium text-text-primary">
              {providerLabel} API Key
            </span>
            <span className={compactPillClass}>
              {hasSavedKey ? '已就绪' : cleared ? '待清除' : '未配置'}
            </span>
          </div>
          <div className="mt-0.5 text-[12px] text-text-muted">
            {hasSavedKey
              ? '已配置（显式密钥优先于环境变量，已保存的密钥不会回显）'
              : '直接填写密钥，或留空使用下方的环境变量'}
          </div>
        </div>
        <div className="flex w-full items-center gap-2 sm:w-auto">
          <input
            type="password"
            className={cn(fieldInput, 'max-w-full sm:w-[260px]')}
            value={apiKey}
            disabled={disabled}
            placeholder={hasSavedKey ? '已配置，留空保留' : '输入 API Key'}
            onChange={(event) => onApiKeyChange(event.target.value)}
          />
          {hasSavedKey && (
            <button
              type="button"
              className={settingsDangerButtonClass}
              disabled={disabled}
              onClick={onClearSavedKey}
            >
              清除
            </button>
          )}
        </div>
      </div>

      <div className={settingsRowClass}>
        <div className="min-w-0">
          <div className="text-[13px] font-medium text-text-primary">
            环境变量名
          </div>
          <div className="mt-0.5 text-[12px] text-text-muted">
            当未设置内联 API Key 时，从该环境变量读取密钥
          </div>
        </div>
        <input
          type="text"
          className={cn(fieldInput, 'max-w-full sm:w-[260px]')}
          value={apiKeyEnv}
          disabled={disabled}
          placeholder={defaultEnvPlaceholder}
          onChange={(event) => onApiKeyEnvChange(event.target.value)}
        />
      </div>
    </>
  )
}
