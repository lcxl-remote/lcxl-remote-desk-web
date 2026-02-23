import React from 'react'
import ReactDOM from 'react-dom/client'
import { AppProviders } from './app/providers'
import './locales/i18n'
import './index.css'

import eruda from 'eruda';

if (/Mobile|Android|iP(ad|hone)/.test(navigator.userAgent)) {
    eruda.init();
}

ReactDOM.createRoot(document.getElementById('root')!).render(
    <React.StrictMode>
        <AppProviders />
    </React.StrictMode>,
)
