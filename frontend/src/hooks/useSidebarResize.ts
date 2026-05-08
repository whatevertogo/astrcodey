import { useState, useCallback, useRef, useEffect } from 'react'

const STORAGE_KEY = 'astrcode-sidebar-width'
const DEFAULT_WIDTH = 260
const MIN_WIDTH = 180
const MAX_WIDTH = 420

export interface UseSidebarResize {
  width: number
  isOpen: boolean
  toggle: () => void
  onResizeStart: (e: React.PointerEvent) => void
  isResizing: boolean
}

export function useSidebarResize(): UseSidebarResize {
  const [width, setWidth] = useState(() => {
    const stored = localStorage.getItem(STORAGE_KEY)
    if (stored) {
      const parsed = Number(stored)
      if (
        Number.isFinite(parsed) &&
        parsed >= MIN_WIDTH &&
        parsed <= MAX_WIDTH
      ) {
        return parsed
      }
    }
    return DEFAULT_WIDTH
  })
  const [isOpen, setIsOpen] = useState(true)
  const [isResizing, setIsResizing] = useState(false)
  const startXRef = useRef(0)
  const startWidthRef = useRef(0)

  const persistWidth = useCallback((nextWidth: number) => {
    try {
      localStorage.setItem(STORAGE_KEY, String(nextWidth))
    } catch {
      // Ignore storage errors
    }
  }, [])

  useEffect(() => {
    if (!isResizing) return

    const handlePointerMove = (e: PointerEvent) => {
      const delta = e.clientX - startXRef.current
      const nextWidth = Math.min(
        MAX_WIDTH,
        Math.max(MIN_WIDTH, startWidthRef.current + delta)
      )
      setWidth(nextWidth)
    }

    const handlePointerUp = () => {
      setIsResizing(false)
      setWidth((current) => {
        persistWidth(current)
        return current
      })
    }

    window.addEventListener('pointermove', handlePointerMove)
    window.addEventListener('pointerup', handlePointerUp)
    return () => {
      window.removeEventListener('pointermove', handlePointerMove)
      window.removeEventListener('pointerup', handlePointerUp)
    }
  }, [isResizing, persistWidth])

  const onResizeStart = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault()
      startXRef.current = e.clientX
      startWidthRef.current = width
      setIsResizing(true)
    },
    [width]
  )

  const toggle = useCallback(() => {
    setIsOpen((v) => !v)
  }, [])

  return { width, isOpen, toggle, onResizeStart, isResizing }
}
