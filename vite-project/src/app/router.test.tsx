import { describe, expect, it } from 'vitest'

import { router } from './router'

type RouteNode = {
    lazy?: () => Promise<Record<string, unknown>>
    children?: RouteNode[]
}

function collectLazyRoutes(routes: RouteNode[]): Array<NonNullable<RouteNode['lazy']>> {
    return routes.flatMap((route) => [
        ...(route.lazy ? [route.lazy] : []),
        ...collectLazyRoutes(route.children ?? []),
    ])
}

describe('router lazy modules', () => {
    it('resolve a component for every lazy route', async () => {
        const lazyRoutes = collectLazyRoutes(router.routes as RouteNode[])
        const modules = await Promise.all(lazyRoutes.map((load) => load()))

        expect(lazyRoutes.length).toBeGreaterThan(0)
        for (const module of modules) {
            expect(module.Component).toBeTypeOf('function')
        }
    })
})
