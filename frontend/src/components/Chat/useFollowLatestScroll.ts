import {
  useCallback,
  useEffect,
  useRef,
  type RefObject,
  type TouchEvent,
  type WheelEvent,
} from 'react'
import {
  isNearBottom as isNearBottomPx,
  nextStickToBottom,
} from './scrollStickiness'

interface UseFollowLatestScrollOptions {
  listRef: RefObject<HTMLDivElement | null>
  contentRef: RefObject<HTMLDivElement | null>
  itemCount: number
  sessionId: string | null
  streamingBlockId: string | null
}

function isNearBottom(container: HTMLDivElement) {
  return isNearBottomPx(
    container.scrollTop,
    container.scrollHeight,
    container.clientHeight
  )
}

export function useFollowLatestScroll({
  listRef,
  contentRef,
  itemCount,
  sessionId,
  streamingBlockId,
}: UseFollowLatestScrollOptions) {
  const shouldStickRef = useRef(true)
  const prevItemCountRef = useRef(0)
  const lastScrollTopRef = useRef(0)
  const ignoreScrollRef = useRef(false)
  const touchStartYRef = useRef<number | null>(null)
  const programmaticScrollFrameRef = useRef(0)

  const markUserScrolledUp = useCallback(() => {
    shouldStickRef.current = false
  }, [])

  const syncStickinessFromCurrentPosition = useCallback(() => {
    const container = listRef.current
    if (!container) return shouldStickRef.current
    if (isNearBottom(container)) {
      shouldStickRef.current = true
    }
    return shouldStickRef.current
  }, [listRef])

  const runProgrammaticScroll = useCallback(
    (behavior: ScrollBehavior = 'auto') => {
      const container = listRef.current
      if (!container) return

      ignoreScrollRef.current = true
      container.scrollTo({ top: container.scrollHeight, behavior })

      if (programmaticScrollFrameRef.current) {
        cancelAnimationFrame(programmaticScrollFrameRef.current)
      }
      programmaticScrollFrameRef.current = requestAnimationFrame(() => {
        lastScrollTopRef.current = container.scrollTop
        ignoreScrollRef.current = false
        programmaticScrollFrameRef.current = 0
      })
    },
    [listRef]
  )

  const followLatest = useCallback(
    (behavior: ScrollBehavior = 'auto') => {
      if (!syncStickinessFromCurrentPosition()) return
      runProgrammaticScroll(behavior)
    },
    [runProgrammaticScroll, syncStickinessFromCurrentPosition]
  )

  const handleScroll = useCallback(() => {
    if (ignoreScrollRef.current) return

    const container = listRef.current
    if (!container) return

    const scrollTop = container.scrollTop
    shouldStickRef.current = nextStickToBottom(
      shouldStickRef.current,
      scrollTop,
      lastScrollTopRef.current,
      isNearBottom(container)
    )
    lastScrollTopRef.current = scrollTop
  }, [listRef])

  const handleWheel = useCallback(
    (event: WheelEvent<HTMLDivElement>) => {
      if (event.deltaY < 0) {
        markUserScrolledUp()
      }
    },
    [markUserScrolledUp]
  )

  const handleTouchStart = useCallback((event: TouchEvent<HTMLDivElement>) => {
    touchStartYRef.current = event.touches[0]?.clientY ?? null
  }, [])

  const handleTouchMove = useCallback(
    (event: TouchEvent<HTMLDivElement>) => {
      const startY = touchStartYRef.current
      const currentY = event.touches[0]?.clientY
      if (startY === null || currentY === undefined) return
      if (currentY > startY + 4) {
        markUserScrolledUp()
      }
    },
    [markUserScrolledUp]
  )

  useEffect(() => {
    shouldStickRef.current = true
    prevItemCountRef.current = 0
    lastScrollTopRef.current = 0
  }, [sessionId])

  useEffect(() => {
    const isFirstPaint = prevItemCountRef.current === 0 && itemCount > 0
    const grew = itemCount > prevItemCountRef.current
    prevItemCountRef.current = itemCount

    if (!grew && !isFirstPaint) return
    const shouldFollow = syncStickinessFromCurrentPosition() || isFirstPaint
    if (!shouldFollow) return

    const frame = requestAnimationFrame(() => {
      if (!syncStickinessFromCurrentPosition() && !isFirstPaint) return
      if (itemCount === 0) return
      followLatest()
    })
    return () => cancelAnimationFrame(frame)
  }, [itemCount, followLatest, syncStickinessFromCurrentPosition])

  useEffect(() => {
    if (!streamingBlockId) return
    const content = contentRef.current
    if (!content) return

    if (syncStickinessFromCurrentPosition()) {
      followLatest()
    }

    let frame = 0
    const scheduleFollowLatest = () => {
      if (!syncStickinessFromCurrentPosition()) return
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(() => {
        if (!syncStickinessFromCurrentPosition()) return
        followLatest()
      })
    }
    const resizeObserver = new ResizeObserver(scheduleFollowLatest)
    const mutationObserver = new MutationObserver(scheduleFollowLatest)
    resizeObserver.observe(content)
    mutationObserver.observe(content, {
      childList: true,
      characterData: true,
      subtree: true,
    })
    return () => {
      cancelAnimationFrame(frame)
      resizeObserver.disconnect()
      mutationObserver.disconnect()
    }
  }, [
    contentRef,
    streamingBlockId,
    followLatest,
    syncStickinessFromCurrentPosition,
  ])

  useEffect(
    () => () => {
      if (programmaticScrollFrameRef.current) {
        cancelAnimationFrame(programmaticScrollFrameRef.current)
      }
    },
    []
  )

  return {
    handleScroll,
    handleWheel,
    handleTouchStart,
    handleTouchMove,
  }
}
