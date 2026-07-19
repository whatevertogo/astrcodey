import type { ReactNode } from 'react'
import { fieldInput } from '../../lib/styles'
import { Button, Modal } from '../ui'
import type {
  PendingOperation,
  ProviderConfigDialogState,
  ProviderDialogState,
  ProviderRemoveDialogState,
} from './settingsSupport'

interface ProviderDialogsProps {
  dialog: ProviderDialogState | null
  operation: PendingOperation | null
  activeProfileName: string
  onConfigChange: (patch: Partial<ProviderConfigDialogState>) => void
  onClose: () => void
  onApply: (dialog: ProviderConfigDialogState, activate: boolean) => void
  onRemove: (dialog: ProviderRemoveDialogState) => void
}

export function ProviderDialogs({
  dialog,
  operation,
  activeProfileName,
  onConfigChange,
  onClose,
  onApply,
  onRemove,
}: ProviderDialogsProps) {
  if (!dialog) return null

  if (dialog.kind === 'remove') {
    const removing = operation?.kind === 'remove-profile'
    return (
      <Modal
        title={`移除 ${dialog.value.provider?.displayName ?? dialog.value.profile.name} 配置`}
        onClose={() => {
          if (!removing) onClose()
        }}
        className="w-[460px]"
      >
        <div className="space-y-4">
          <p className="text-[13px] leading-relaxed text-text-secondary">
            这会删除 {dialog.value.profile.name} 的 Base URL、API Key
            和模型配置。
          </p>
          {dialog.value.profile.name === activeProfileName && (
            <p className="rounded-lg border border-warning/20 bg-warning-soft px-3 py-2 text-[13px] text-warning">
              当前正在使用这个 Provider，移除后会切换到其他可用配置。
            </p>
          )}
          <div className="flex justify-end gap-2 pt-2">
            <Button variant="secondary" disabled={removing} onClick={onClose}>
              返回
            </Button>
            <Button
              variant="danger"
              disabled={removing}
              onClick={() => onRemove(dialog.value)}
            >
              {removing ? '移除中...' : '移除配置'}
            </Button>
          </div>
        </div>
      </Modal>
    )
  }

  const config = dialog.value
  const applying = operation?.kind === 'apply-provider'
  const canSubmit = !applying && Boolean(config.baseUrl.trim())
  return (
    <Modal
      title={`${config.existingProfile ? '编辑' : '配置'} ${config.provider.displayName}`}
      onClose={() => {
        if (!applying) onClose()
      }}
      className="w-[520px]"
    >
      <form
        className="space-y-4"
        onSubmit={(event) => {
          event.preventDefault()
          onApply(config, true)
        }}
      >
        <DialogField label="Base URL">
          <input
            className={fieldInput}
            value={config.baseUrl}
            placeholder="https://api.example.com/v1"
            disabled={applying}
            onChange={(event) =>
              onConfigChange({ baseUrl: event.target.value })
            }
          />
        </DialogField>

        <DialogField label="API Key">
          <input
            className={fieldInput}
            type="password"
            value={config.apiKey}
            placeholder={
              config.existingProfile?.hasApiKey
                ? '已配置，留空保留'
                : config.provider.apiKeyEnvVars[0]
                  ? `API Key 或 env:${config.provider.apiKeyEnvVars[0]}`
                  : 'API Key'
            }
            disabled={applying}
            onChange={(event) => onConfigChange({ apiKey: event.target.value })}
          />
          {config.existingProfile?.hasApiKey && (
            <div className="mt-2 text-[12px] text-text-muted">
              已保存的 Key 不会显示。
            </div>
          )}
        </DialogField>

        <DialogField label="Model">
          <input
            className={fieldInput}
            value={config.modelId}
            placeholder={config.provider.defaultModel}
            disabled={applying}
            onChange={(event) =>
              onConfigChange({ modelId: event.target.value })
            }
          />
        </DialogField>

        <div className="flex justify-end gap-2 pt-2">
          <Button variant="secondary" disabled={applying} onClick={onClose}>
            取消
          </Button>
          <Button
            variant="secondary"
            disabled={!canSubmit}
            onClick={() => onApply(config, false)}
          >
            仅保存
          </Button>
          <Button variant="primary" disabled={!canSubmit} type="submit">
            {applying ? '保存中...' : '保存并使用'}
          </Button>
        </div>
      </form>
    </Modal>
  )
}

function DialogField({
  label,
  children,
}: {
  label: string
  children: ReactNode
}) {
  return (
    <div>
      <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
        {label}
      </label>
      {children}
    </div>
  )
}
