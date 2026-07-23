import React from 'react'
import ReactDOM from 'react-dom/client'
import { bootstrapApplication } from './app/bootstrap'
import { AppProviders } from './app/providers'
import { initializeI18n } from './locales/i18n'
import { initializeNativeLocaleBridge } from './locales/native-locale'
import './index.css'

const root = ReactDOM.createRoot(document.getElementById('root')!)

void bootstrapApplication({
    root,
    initialize: async () => {
        await initializeI18n()
        initializeNativeLocaleBridge()

        if (
            import.meta.env.DEV &&
            /Mobile|Android|iP(ad|hone)/.test(navigator.userAgent)
        ) {
            void import('eruda').then(({ default: eruda }) => eruda.init())
        }
    },
    application: (
        <React.StrictMode>
            <AppProviders />
        </React.StrictMode>
    ),
})
