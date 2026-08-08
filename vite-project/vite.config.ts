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
                changeOrigin: true,
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
