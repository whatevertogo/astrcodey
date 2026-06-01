import { useEffect, useState, useMemo, useCallback } from 'react'
import type {
  ConfigView,
  ExtensionStateView,
  ProfileView,
  ModelTestResult,
} from '../../services/types'
import { btnPrimary, fieldInput } from '../../lib/styles'
import { getStoredTheme, setTheme, type ThemePreference } from '../../lib/theme'
import { Modal, Button } from '../ui'
import * as api from '../../services/api'

type SettingsTab = 'model' | 'extensions' | 'appearance'

const THEME_OPTIONS: { value: ThemePreference; label: string; hint: string }[] =
  [
    { value: 'light', label: '浅色', hint: '始终使用浅色界面' },
    { value: 'dark', label: '深色', hint: '始终使用深色界面' },
    { value: 'system', label: '跟随系统', hint: '自动匹配系统外观设置' },
  ]

interface SettingsModalProps {
  onClose: () => void
  getConfig: () => Promise<ConfigView>
  reloadConfig: () => Promise<void>
  saveActiveSelection: (
    profile: string,
    model: string,
    smallProfile?: string,
    smallModel?: string,
    approvalMode?: 'manual' | 'yolo'
  ) => Promise<void>
  testConnection: () => Promise<ModelTestResult>
  extensions: ExtensionStateView[]
  onRefreshExtensions: () => Promise<void>
}

function pickModel(
  profile: ProfileView | undefined,
  currentModel: string
): string {
  if (!profile || profile.models.length === 0) return ''
  if (profile.models.some((m) => m.id === currentModel)) return currentModel
  return profile.models[0]?.id ?? ''
}

function sourceLabel(source: ExtensionStateView['source']): string {
  switch (source) {
    case 'builtin':
      return '内置'
    case 'disk':
      return '磁盘'
    default:
      return '未知'
  }
}

export default function SettingsModal({
  onClose,
  getConfig,
  reloadConfig,
  saveActiveSelection,
  testConnection,
  extensions,
  onRefreshExtensions,
}: SettingsModalProps) {
  const [tab, setTab] = useState<SettingsTab>('model')
  const [configView, setConfigView] = useState<ConfigView | null>(null)
  const [selectedProfile, setSelectedProfile] = useState('')
  const [selectedModel, setSelectedModel] = useState('')
  const [selectedSmallProfile, setSelectedSmallProfile] = useState('')
  const [selectedSmallModel, setSelectedSmallModel] = useState('')
  const [yoloEnabled, setYoloEnabled] = useState(false)
  const [testResult, setTestResult] = useState<ModelTestResult | null>(null)
  const [loading, setLoading] = useState(true)
  const [testing, setTesting] = useState(false)
  const [saving, setSaving] = useState(false)
  const [reloading, setReloading] = useState(false)
  const [extensionBusy, setExtensionBusy] = useState<string | null>(null)
  const [errorMessage, setErrorMessage] = useState<string | null>(null)
  const [themePreference, setThemePreference] =
    useState<ThemePreference>(getStoredTheme)

  useEffect(() => {
    let cancelled = false
    const load = async () => {
      setLoading(true)
      try {
        const cfg = await getConfig()
        if (cancelled) return
        setConfigView(cfg)
        setSelectedProfile(cfg.activeProfile)
        setSelectedModel(cfg.activeModel)
        setSelectedSmallProfile(cfg.activeSmallProfile ?? '')
        setSelectedSmallModel(cfg.activeSmallModel ?? '')
        setYoloEnabled(cfg.approvalMode === 'yolo')
      } catch (err) {
        if (!cancelled) setErrorMessage(String(err))
      } finally {
        if (!cancelled) setLoading(false)
      }
    }
    void load()
    return () => {
      cancelled = true
    }
  }, [getConfig])

  const profiles = useMemo(() => configView?.profiles ?? [], [configView])
  const currentProfile = useMemo(
    () => profiles.find((p) => p.name === selectedProfile) ?? profiles[0],
    [profiles, selectedProfile]
  )

  const handleSave = async () => {
    if (!selectedProfile || !selectedModel) return
    setSaving(true)
    setErrorMessage(null)
    try {
      await saveActiveSelection(
        selectedProfile,
        selectedModel,
        selectedSmallProfile || undefined,
        selectedSmallModel || undefined,
        yoloEnabled ? 'yolo' : 'manual'
      )
      onClose()
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setSaving(false)
    }
  }

  const handleTest = async () => {
    setTesting(true)
    setErrorMessage(null)
    setTestResult(null)
    try {
      const result = await testConnection()
      setTestResult(result)
    } catch (err) {
      setTestResult({ success: false, message: String(err) })
    } finally {
      setTesting(false)
    }
  }

  const handleReload = async () => {
    setReloading(true)
    setErrorMessage(null)
    try {
      await reloadConfig()
      const cfg = await getConfig()
      setConfigView(cfg)
      setSelectedProfile(cfg.activeProfile)
      setSelectedModel(cfg.activeModel)
      setSelectedSmallProfile(cfg.activeSmallProfile ?? '')
      setSelectedSmallModel(cfg.activeSmallModel ?? '')
      setYoloEnabled(cfg.approvalMode === 'yolo')
      setTestResult(null)
      await onRefreshExtensions()
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setReloading(false)
    }
  }

  const handleToggleExtension = useCallback(
    async (extensionId: string, enabled: boolean) => {
      setExtensionBusy(extensionId)
      setErrorMessage(null)
      try {
        const result = await api.setExtensionEnabled(extensionId, enabled)
        if (result.reloadErrors.length > 0) {
          setErrorMessage(result.reloadErrors.join('; '))
        }
        await onRefreshExtensions()
      } catch (err) {
        setErrorMessage(String(err))
      } finally {
        setExtensionBusy(null)
      }
    },
    [onRefreshExtensions]
  )

  const handleReloadExtensions = useCallback(async () => {
    setReloading(true)
    setErrorMessage(null)
    try {
      const result = await api.reloadExtensions()
      if (result.reloadErrors.length > 0) {
        setErrorMessage(result.reloadErrors.join('; '))
      }
      await onRefreshExtensions()
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setReloading(false)
    }
  }, [onRefreshExtensions])

  const handleThemeChange = useCallback((preference: ThemePreference) => {
    setThemePreference(preference)
    setTheme(preference)
  }, [])

  return (
    <Modal
      title="设置"
      onClose={onClose}
      className="max-h-[85vh] overflow-y-auto"
    >
      <div className="mb-4 flex gap-1 rounded-xl border border-border bg-surface-soft p-1">
        <button
          type="button"
          className={`flex-1 rounded-lg px-3 py-2 text-[13px] font-semibold transition-colors ${
            tab === 'model'
              ? 'bg-surface text-text-primary shadow-soft'
              : 'text-text-secondary hover:text-text-primary'
          }`}
          onClick={() => setTab('model')}
        >
          模型
        </button>
        <button
          type="button"
          className={`flex-1 rounded-lg px-3 py-2 text-[13px] font-semibold transition-colors ${
            tab === 'extensions'
              ? 'bg-surface text-text-primary shadow-soft'
              : 'text-text-secondary hover:text-text-primary'
          }`}
          onClick={() => setTab('extensions')}
        >
          扩展
        </button>
        <button
          type="button"
          className={`flex-1 rounded-lg px-3 py-2 text-[13px] font-semibold transition-colors ${
            tab === 'appearance'
              ? 'bg-surface text-text-primary shadow-soft'
              : 'text-text-secondary hover:text-text-primary'
          }`}
          onClick={() => setTab('appearance')}
        >
          外观
        </button>
      </div>

      {loading ? (
        <div className="flex items-center gap-2 text-xs text-text-secondary">
          <span className="h-[14px] w-[14px] animate-spin rounded-full border-2 border-border border-t-text-secondary" />
          加载中...
        </div>
      ) : tab === 'model' ? (
        <>
          <div className="mb-4">
            <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
              配置文件
            </label>
            <div className="overflow-hidden text-ellipsis whitespace-nowrap rounded-xl border border-border bg-surface px-3 py-[11px] text-xs text-text-primary">
              {configView?.configPath ?? ''}
            </div>
          </div>

          <div className="mb-4">
            <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
              Profile
            </label>
            <select
              className={fieldInput}
              value={selectedProfile}
              onChange={(e) => {
                const name = e.target.value
                const profile = profiles.find((p) => p.name === name)
                setSelectedProfile(name)
                setSelectedModel(pickModel(profile, selectedModel))
                setTestResult(null)
                setErrorMessage(null)
              }}
            >
              {profiles.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name}
                </option>
              ))}
            </select>
          </div>

          <div className="mb-4">
            <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
              Model
            </label>
            <select
              className={fieldInput}
              value={selectedModel}
              onChange={(e) => {
                setSelectedModel(e.target.value)
                setTestResult(null)
                setErrorMessage(null)
              }}
              disabled={!currentProfile || currentProfile.models.length === 0}
            >
              {currentProfile?.models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.id}
                </option>
              ))}
            </select>
          </div>

          <div className="mb-4">
            <label className="mb-2 block text-[13px] font-semibold text-text-secondary">
              Small Model
            </label>
            <select
              className={`${fieldInput} mb-2`}
              value={selectedSmallProfile}
              onChange={(e) => {
                const name = e.target.value
                const profile = profiles.find((p) => p.name === name)
                setSelectedSmallProfile(name)
                setSelectedSmallModel(
                  name && profile?.models.length ? profile.models[0].id : ''
                )
                setTestResult(null)
                setErrorMessage(null)
              }}
            >
              <option value="">不使用</option>
              {profiles.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name}
                </option>
              ))}
            </select>
            {selectedSmallProfile && (
              <select
                className={fieldInput}
                value={selectedSmallModel}
                onChange={(e) => {
                  setSelectedSmallModel(e.target.value)
                  setTestResult(null)
                  setErrorMessage(null)
                }}
                disabled={
                  !profiles.find((p) => p.name === selectedSmallProfile)?.models
                    .length
                }
              >
                {profiles
                  .find((p) => p.name === selectedSmallProfile)
                  ?.models.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.id}
                    </option>
                  ))}
              </select>
            )}
          </div>

          <div className="mb-4 rounded-xl border border-border px-3 py-3">
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <p className="text-[13px] font-semibold text-text-primary">
                  YOLO 模式
                </p>
                <p className="mt-0.5 text-[12px] leading-relaxed text-text-muted">
                  开启后自动批准工具调用，跳过 Shell、写入等操作的审批提示
                </p>
              </div>
              <label className="inline-flex shrink-0 cursor-pointer items-center gap-2 text-xs text-text-secondary">
                <input
                  type="checkbox"
                  checked={yoloEnabled}
                  onChange={(e) => {
                    setYoloEnabled(e.target.checked)
                    setTestResult(null)
                    setErrorMessage(null)
                  }}
                  className="h-4 w-4 accent-accent-strong"
                />
                {yoloEnabled ? '已开启' : '已关闭'}
              </label>
            </div>
          </div>

          <div className="mb-4 divide-y divide-border rounded-xl border border-border">
            <div className="flex justify-between px-3 py-2.5">
              <span className="text-xs text-text-secondary">Base URL</span>
              <span className="break-all text-xs text-text-primary">
                {currentProfile?.baseUrl ?? '-'}
              </span>
            </div>
            <div className="flex justify-between px-3 py-2.5">
              <span className="text-xs text-text-secondary">API Key</span>
              <span className="text-xs text-text-primary">
                {currentProfile?.hasApiKey ? '已配置' : '未配置'}
              </span>
            </div>
          </div>

          <div className="mt-[22px] flex justify-end gap-2.5">
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

          {testResult && (
            <div
              className={`mt-3 text-xs ${testResult.success ? 'text-success' : 'text-danger'}`}
            >
              {testResult.success ? '连接成功' : '连接失败'}:{' '}
              {testResult.message}
            </div>
          )}
        </>
      ) : tab === 'appearance' ? (
        <>
          <p className="mb-4 text-[13px] text-text-secondary">
            选择界面配色方案
          </p>
          <div className="divide-y divide-border rounded-xl border border-border">
            {THEME_OPTIONS.map((option) => (
              <label
                key={option.value}
                className="flex cursor-pointer items-center justify-between gap-3 px-3 py-3 transition-colors hover:bg-surface-muted"
              >
                <div className="min-w-0">
                  <p className="text-[13px] font-medium text-text-primary">
                    {option.label}
                  </p>
                  <p className="mt-0.5 text-[12px] text-text-muted">
                    {option.hint}
                  </p>
                </div>
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
        </>
      ) : (
        <>
          <div className="mb-3 flex items-center justify-between gap-2">
            <p className="text-[13px] text-text-secondary">
              管理已安装的扩展及其启用状态
            </p>
            <Button
              variant="secondary"
              onClick={() => void handleReloadExtensions()}
              disabled={reloading}
            >
              {reloading ? '重载中...' : '重载扩展'}
            </Button>
          </div>

          {extensions.length === 0 ? (
            <div className="rounded-xl border border-dashed border-border px-4 py-6 text-center text-sm text-text-secondary">
              暂无扩展
            </div>
          ) : (
            <div className="divide-y divide-border rounded-xl border border-border">
              {extensions.map((ext) => (
                <div
                  key={ext.extensionId}
                  className="flex items-center justify-between gap-3 px-3 py-3"
                >
                  <div className="min-w-0">
                    <div className="truncate text-[13px] font-medium text-text-primary">
                      {ext.extensionId}
                    </div>
                    <div className="mt-0.5 text-[11px] text-text-secondary">
                      {sourceLabel(ext.source)}
                      {ext.loaded ? ' · 已加载' : ' · 未加载'}
                    </div>
                  </div>
                  <label className="inline-flex shrink-0 cursor-pointer items-center gap-2 text-xs text-text-secondary">
                    <input
                      type="checkbox"
                      checked={ext.enabled}
                      disabled={extensionBusy === ext.extensionId}
                      onChange={(e) =>
                        void handleToggleExtension(
                          ext.extensionId,
                          e.target.checked
                        )
                      }
                      className="h-4 w-4 accent-accent-strong"
                    />
                    {ext.enabled ? '已启用' : '已禁用'}
                  </label>
                </div>
              ))}
            </div>
          )}
        </>
      )}

      {errorMessage && (
        <div className="mt-3 text-xs text-danger">{errorMessage}</div>
      )}
    </Modal>
  )
}
