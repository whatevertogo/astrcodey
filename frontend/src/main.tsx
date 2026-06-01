import React from 'react'
import ReactDOM from 'react-dom/client'
import './index.css'
import { initTheme } from './lib/theme'
import App from './App'

initTheme()

const rootElement = document.getElementById('root')
if (!rootElement) throw new Error('Root element "#root" not found')

ReactDOM.createRoot(rootElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
)
