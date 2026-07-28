import {
  Component,
  lazy,
  memo,
  Suspense,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import {
  cachedStreamingMarkdownSplit,
  safeStreamingMarkdownCommit,
} from './markdownStreaming'

const MarkdownRenderer = lazy(() => import('./MarkdownRenderer'))

class MarkdownBoundary extends Component<
  { fallback: string; children: ReactNode },
  { hasError: boolean; renderedFallback: string }
> {
  state = {
    hasError: false,
    renderedFallback: this.props.fallback,
  }

  static getDerivedStateFromProps(
    props: { fallback: string },
    state: { hasError: boolean; renderedFallback: string }
  ) {
    if (props.fallback === state.renderedFallback) return null
    return {
      hasError: false,
      renderedFallback: props.fallback,
    }
  }

  static getDerivedStateFromError() {
    return { hasError: true }
  }

  render() {
    if (this.state.hasError) {
      return (
        <pre className="m-0 whitespace-pre-wrap overflow-wrap-anywhere font-inherit text-inherit">
          {this.props.fallback}
        </pre>
      )
    }
    return this.props.children
  }
}

export const MarkdownContent = memo(function MarkdownContent({
  text,
}: {
  text: string
}) {
  return (
    <MarkdownBoundary fallback={text}>
      <Suspense
        fallback={
          <span className="whitespace-pre-wrap break-words">{text}</span>
        }
      >
        <MarkdownRenderer text={text} />
      </Suspense>
    </MarkdownBoundary>
  )
})

const StreamingCursor = () => (
  <span className="ml-px inline-block animate-blink text-text-secondary motion-reduce:animate-none">
    ▋
  </span>
)

const STREAMING_MARKDOWN_RENDER_INTERVAL_MS = 100

function useThrottledStreamingText(text: string): string {
  const [rendered, setRendered] = useState(text)
  const latestRef = useRef(text)
  const timeoutRef = useRef<number | null>(null)

  useEffect(() => {
    latestRef.current = text
    if (rendered === text || timeoutRef.current !== null) return

    timeoutRef.current = window.setTimeout(() => {
      timeoutRef.current = null
      setRendered((current) =>
        current === latestRef.current ? current : latestRef.current
      )
    }, STREAMING_MARKDOWN_RENDER_INTERVAL_MS)
  }, [rendered, text])

  useEffect(
    () => () => {
      if (timeoutRef.current !== null) {
        window.clearTimeout(timeoutRef.current)
      }
    },
    []
  )

  return rendered
}

/** Streaming 时：已稳定部分走 ReactMarkdown，未完成尾巴纯文本。 */
function StreamingMarkdownContent({
  text,
  cacheKey,
}: {
  text: string
  cacheKey: string
}) {
  const split = cachedStreamingMarkdownSplit(cacheKey, text)
  const hasCommit = split.commitIndex !== -1
  const renderedCommit = useThrottledStreamingText(split.committed)
  const safeRenderedCommit = safeStreamingMarkdownCommit(
    split.committed,
    renderedCommit
  )
  const liveTail = text.slice(safeRenderedCommit.length)

  if (!hasCommit) {
    return (
      <>
        <span className="whitespace-pre-wrap break-words">{text}</span>
        <StreamingCursor />
      </>
    )
  }

  return (
    <>
      {safeRenderedCommit ? (
        <MarkdownContent text={safeRenderedCommit} />
      ) : null}
      {liveTail ? (
        <span className="whitespace-pre-wrap break-words">{liveTail}</span>
      ) : null}
      <StreamingCursor />
    </>
  )
}

export function StreamingMarkdown(props: { text: string; cacheKey: string }) {
  return <StreamingMarkdownContent key={props.cacheKey} {...props} />
}
