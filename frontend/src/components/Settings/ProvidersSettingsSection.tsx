import {
  providerAuthSchemeLabel,
  providerWireFormatLabel,
} from '../../lib/providerLabels'
import { cn } from '../../lib/utils'
import type {
  ProfileView,
  ProviderAuthScheme,
  ProviderSpecView,
  ProviderWireFormat,
} from '../../services/types'
import { Button } from '../ui'
import {
  compactPillClass,
  findProviderProfile,
  type PendingOperation,
  type ProviderRemoveDialogState,
  quietButtonClass,
  settingsDangerButtonClass,
  settingsDividerClass,
  settingsPanelClass,
} from './settingsSupport'

interface ProvidersSettingsSectionProps {
  profiles: ProfileView[]
  providers: ProviderSpecView[]
  activeProfileName: string
  selectedModelId: string
  operation: PendingOperation | null
  onReload: () => void
  onActivate: (profile: ProfileView) => void
  onConfigure: (provider: ProviderSpecView, profile?: ProfileView) => void
  onRemove: (dialog: ProviderRemoveDialogState) => void
}

interface ProviderMetadataProps {
  providerKind: string
  wireFormat: ProviderWireFormat
  authScheme: ProviderAuthScheme
}

function ProviderMetadata({
  providerKind,
  wireFormat,
  authScheme,
}: ProviderMetadataProps) {
  return (
    <div className="mt-1 flex min-w-0 flex-wrap gap-1.5 text-[11px] text-text-muted">
      <span>{providerKind}</span>
      <span>·</span>
      <span>{providerWireFormatLabel(wireFormat)}</span>
      <span>·</span>
      <span>{providerAuthSchemeLabel(authScheme)}</span>
    </div>
  )
}

function configuredModel(profile: ProfileView, selectedModelId: string) {
  return (
    profile.models.find((model) => model.id === selectedModelId)?.id ??
    profile.models[0]?.id
  )
}

export function ProvidersSettingsSection({
  profiles,
  providers,
  activeProfileName,
  selectedModelId,
  operation,
  onReload,
  onActivate,
  onConfigure,
  onRemove,
}: ProvidersSettingsSectionProps) {
  const canMutate = operation === null
  const reloading = operation?.kind === 'reload'

  return (
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
            onClick={onReload}
            disabled={!canMutate}
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
            const modelId = configuredModel(profile, selectedModelId)
            const isActive = profile.name === activeProfileName
            const isActivating =
              operation?.kind === 'activate-profile' &&
              operation.profileName === profile.name
            const isRemoving =
              operation?.kind === 'remove-profile' &&
              operation.profileName === profile.name

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
                  <ProviderMetadata
                    providerKind={profile.providerKind}
                    wireFormat={profile.wireFormat}
                    authScheme={profile.authScheme}
                  />
                </div>
                <div className="min-w-0 space-y-1 text-[12px]">
                  <div className="truncate text-text-primary">
                    {modelId ?? '-'}
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
                      disabled={!canMutate || !modelId}
                      onClick={() => onActivate(profile)}
                    >
                      {isActivating ? '切换中...' : '设为当前'}
                    </Button>
                  )}
                  <Button
                    variant="danger"
                    className={settingsDangerButtonClass}
                    disabled={!canMutate}
                    onClick={() => onRemove({ profile })}
                  >
                    {isRemoving ? '移除中...' : '移除'}
                  </Button>
                </div>
              </div>
            )
          })
        )}
      </div>

      {providers.length > 0 && (
        <div className={cn(settingsPanelClass, settingsDividerClass)}>
          <div className="flex items-center justify-between gap-4 px-4 py-3">
            <div>
              <h2 className="text-[13px] font-semibold text-text-primary">
                Provider Presets
              </h2>
              <div className="mt-0.5 text-[12px] text-text-muted">
                {providers.length} presets
              </div>
            </div>
          </div>
          {providers.map((provider) => {
            const defaultEndpoint = provider.endpoints.find(
              (endpoint) => endpoint.isDefault
            )
            const profile = findProviderProfile(profiles, provider)
            const modelId = profile
              ? configuredModel(profile, selectedModelId)
              : undefined
            const isConfigured = Boolean(profile)
            const isActive = profile?.name === activeProfileName
            const isApplying =
              operation?.kind === 'apply-provider' &&
              operation.providerId === provider.id
            const isActivating =
              operation?.kind === 'activate-profile' &&
              operation.profileName === profile?.name
            const isRemoving =
              operation?.kind === 'remove-profile' &&
              operation.profileName === profile?.name
            const capabilityLabels = [
              provider.capabilities.promptCacheKey ? 'Cache key' : null,
              provider.capabilities.streamUsage ? 'Stream usage' : null,
              provider.capabilities.reasoningEffort ? 'Reasoning' : null,
            ].filter((label): label is string => Boolean(label))
            const displayedBaseUrl =
              profile?.baseUrl ??
              defaultEndpoint?.baseUrl ??
              defaultEndpoint?.label ??
              '-'

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
                      {isActive ? '当前' : isConfigured ? '已配置' : '未配置'}
                    </span>
                  </div>
                  <ProviderMetadata
                    providerKind={provider.providerKind}
                    wireFormat={provider.wireFormat}
                    authScheme={provider.authScheme}
                  />
                  {capabilityLabels.length > 0 && (
                    <div className="mt-2 flex flex-wrap gap-1.5">
                      {capabilityLabels.map((label) => (
                        <span key={label} className={compactPillClass}>
                          {label}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
                <div className="min-w-0 space-y-1 text-[12px]">
                  <div className="truncate text-text-primary">
                    {modelId ?? provider.defaultModel}
                  </div>
                  <div className="truncate text-text-muted">
                    {displayedBaseUrl}
                  </div>
                  <div className="truncate text-text-muted">
                    {profile?.hasApiKey
                      ? 'Key 已配置'
                      : provider.apiKeyEnvVars[0]
                        ? `Key env:${provider.apiKeyEnvVars[0]}`
                        : 'Key 未配置'}
                  </div>
                </div>
                <div className="flex flex-wrap justify-start gap-2 md:justify-end">
                  {profile && !isActive && (
                    <Button
                      variant="secondary"
                      className={quietButtonClass}
                      disabled={!canMutate || !modelId}
                      onClick={() => onActivate(profile)}
                    >
                      {isActivating ? '切换中...' : '设为当前'}
                    </Button>
                  )}
                  {profile && (
                    <Button
                      variant="danger"
                      className={settingsDangerButtonClass}
                      disabled={!canMutate}
                      onClick={() => onRemove({ provider, profile })}
                    >
                      {isRemoving ? '移除中...' : '移除'}
                    </Button>
                  )}
                  <Button
                    variant="secondary"
                    className={quietButtonClass}
                    disabled={!canMutate}
                    onClick={() => onConfigure(provider, profile)}
                  >
                    {isApplying ? '保存中...' : isConfigured ? '编辑' : '配置'}
                  </Button>
                </div>
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
