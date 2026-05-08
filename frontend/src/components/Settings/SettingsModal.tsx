import { useEffect, useState, useMemo } from 'react'
import type {
  ConfigView,
  ProfileView,
  ModelTestResult,
} from '../../services/types'
import {
  btnPrimary,
  btnSecondary,
  dialogSurface,
  fieldInput,
  overlay,
} from '../../lib/styles'

interface SettingsModalProps {
  onClose: () => void
  getConfig: () => Promise<ConfigView>
  reloadConfig: () => Promise<void>
  saveActiveSelection: (profile: string, model: string) => Promise<void>
  testConnection: () => Promise<ModelTestResult>
}

function pickModel(
  profile: ProfileView | undefined,
  currentModel: string
): string {
  if (!profile || profile.models.length === 0) return ''
  if (profile.models.some((m) => m.id === currentModel)) return currentModel
  return profile.models[0]?.id ?? ''
}

export default function SettingsModal({
  onClose,
  getConfig,
  reloadConfig,
  saveActiveSelection,
  testConnection,
}: SettingsModalProps) {
  const [configView, setConfigView] = useState<ConfigView | null>(null)
  const [selectedProfile, setSelectedProfile] = useState('')
  const [selectedModel, setSelectedModel] = useState('')
  const [testResult, setTestResult] = useState<ModelTestResult | null>(null)
  const [loading, setLoading] = useState(true)
  const [testing, setTesting] = useState(false)
  const [saving, setSaving] = useState(false)
  const [reloading, setReloading] = useState(false)
  const [errorMessage, setErrorMessage] = useState<string | null>(null)

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

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [onClose])

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
      await saveActiveSelection(selectedProfile, selectedModel)
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
      setTestResult(null)
    } catch (err) {
      setErrorMessage(String(err))
    } finally {
      setReloading(false)
    }
  }

  return (
    <div
      className={overlay}
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className={dialogSurface}>
        <div className="flex items-center justify-between gap-3 mb-[18px]">
          <div className="text-xl font-bold text-text-primary">设置</div>
          <button
            type="button"
            className="w-8 h-8 rounded-[10px] bg-surface-soft text-text-secondary border border-border text-lg hover:bg-white hover:text-text-primary hover:border-border-strong"
            onClick={onClose}
          >
            x
          </button>
        </div>

        {loading ? (
          <div className="flex items-center gap-2 text-xs text-text-secondary">
            <span className="h-[14px] w-[14px] animate-spin rounded-full border-2 border-border border-t-text-secondary" />
            加载中...
          </div>
        ) : (
          <>
            <div className="mb-4">
              <label className="block mb-2 text-text-secondary text-[13px] font-semibold">
                配置文件
              </label>
              <div className="py-[11px] px-3 rounded-xl border border-border bg-surface text-text-primary text-xs overflow-hidden text-ellipsis whitespace-nowrap">
                {configView?.configPath ?? ''}
              </div>
            </div>

            <div className="mb-4">
              <label className="block mb-2 text-text-secondary text-[13px] font-semibold">
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
              <label className="block mb-2 text-text-secondary text-[13px] font-semibold">
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

            <div className="mb-4 rounded-xl border border-border divide-y divide-border">
              <div className="flex justify-between py-2.5 px-3">
                <span className="text-text-secondary text-xs">Base URL</span>
                <span className="text-text-primary text-xs break-all">
                  {currentProfile?.baseUrl ?? '-'}
                </span>
              </div>
              <div className="flex justify-between py-2.5 px-3">
                <span className="text-text-secondary text-xs">API Key</span>
                <span className="text-text-primary text-xs">
                  {currentProfile?.hasApiKey ? '已配置' : '未配置'}
                </span>
              </div>
            </div>

            <div className="flex justify-end gap-2.5 mt-[22px]">
              <button
                type="button"
                className={btnSecondary}
                onClick={() => void handleReload()}
                disabled={reloading || saving || testing}
              >
                {reloading ? '重载中...' : '从磁盘重载'}
              </button>
              <button
                type="button"
                className={btnSecondary}
                onClick={() => void handleTest()}
                disabled={testing || saving}
              >
                {testing ? '测试中...' : '测试连接'}
              </button>
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
            {errorMessage && (
              <div className="mt-3 text-xs text-danger">{errorMessage}</div>
            )}
          </>
        )}
      </div>
    </div>
  )
}
