import { defineConfig } from '@kubb/core'
import { pluginOas } from '@kubb/plugin-oas'
import { pluginTs } from '@kubb/plugin-ts'
import { pluginReactQuery } from '@kubb/plugin-react-query'
import { pluginClient } from '@kubb/plugin-client'

export default defineConfig({
    root: '.',
    input: {
        path: './openapi.json',
    },
    output: {
        path: './src/services',
        clean: true,
    },
    plugins: [
        pluginOas({
            validate: false,
        }),
        pluginTs({
            output: {
                path: 'types.ts',
            },
        }),
        pluginClient({
            output: {
                path: './clients.ts',
            },
            group: {
                type: 'tag',
            },
        }),
        pluginReactQuery({
            output: {
                path: './hooks',
            },
            group: {
                type: 'tag',
            },
            client: {
                importPath: '@/lib/kubb-client',
            },
        }),
    ],
})
