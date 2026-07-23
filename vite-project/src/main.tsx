import React from 'react'
import ReactDOM from 'react-dom/client'
import { AppProviders } from './app/providers'
import './locales/i18n'
import { initializeNativeLocaleBridge } from './locales/native-locale'
import './index.css'

initializeNativeLocaleBridge()

if (/Mobile|Android|iP(ad|hone)/.test(navigator.userAgent)) {
    void import('eruda').then(({ default: eruda }) => eruda.init())
}

ReactDOM.createRoot(document.getElementById('root')!).render(
    <React.StrictMode>
        <AppProviders />
    </React.StrictMode>,
)
