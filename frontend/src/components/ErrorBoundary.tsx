import { Component } from 'react'

interface Props {
  children: React.ReactNode
}

interface State {
  error: Error | null
}

export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex h-full w-full items-center justify-center bg-panel-bg">
          <div className="text-center max-w-md px-6">
            <div className="mb-2 text-[15px] font-semibold text-danger">
              渲染出错
            </div>
            <div className="mb-4 text-[13px] text-text-secondary wrap-break-word">
              {this.state.error.message}
            </div>
            <button
              type="button"
              className="rounded-xl border border-border bg-surface px-4 py-2 text-[13px] font-semibold text-text-primary hover:bg-white"
              onClick={() => this.setState({ error: null })}
            >
              重试
            </button>
          </div>
        </div>
      )
    }
    return this.props.children
  }
}
