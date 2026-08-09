/// <reference types="vitest" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'node:path'

// https://vitejs.dev/config/
export default defineConfig({
    plugins: [react()],
    resolve: {
        alias: {
            "@": path.resolve(import.meta.dirname, "./src"),
        },
    },
    test: {
        globals: true,
        environment: 'jsdom',
        setupFiles: './src/test/setup.ts',
    },
    server: {
        port: 5174,
        proxy: {
            '/api': {
                target: 'http://localhost:8081/',
                // Keep the browser-facing Host so local permission endpoints can
                // verify Origin === Host. Rewriting it to the backend target makes
                // a Tauri dev origin such as 127.0.0.1:5174 look cross-origin.
                changeOrigin: false,
                ws: true,
                xfwd: true,
            }
        },
        host: '0.0.0.0',
        allowedHosts: [
            '.lcxbox.com',
        ],
    },
})
