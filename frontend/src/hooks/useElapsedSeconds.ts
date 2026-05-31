import { useEffect, useState } from 'react'

/** 从 active 变为 true 起每秒递增，active 为 false 时归零。 */
export function useElapsedSeconds(active: boolean): number {
  const [elapsed, setElapsed] = useState(0)

  useEffect(() => {
    if (!active) {
      return
    }

    const start = Date.now()
    const id = window.setInterval(() => {
      setElapsed(Math.floor((Date.now() - start) / 1000))
    }, 1000)
    return () => window.clearInterval(id)
  }, [active])

  return active ? elapsed : 0
}

export function runningElapsedLabel(
  elapsed: number,
  locale: 'en' | 'zh' = 'en'
): string {
  if (locale === 'zh') {
    return elapsed > 0 ? `运行中 ${elapsed}s` : '运行中'
  }
  return elapsed > 0 ? `Running... ${elapsed}s` : 'Running...'
}
