import { describe, expect, it } from 'vitest'
import { requireOpenApiInputPath } from './openapi-input.ts'

describe('requireOpenApiInputPath', () => {
    it('returns the temporary spec path', () => {
        expect(
            requireOpenApiInputPath({
                KUBB_OPENAPI_PATH: '  /tmp/desk-openapi.json  ',
            }),
        ).toBe('/tmp/desk-openapi.json')
    })

    it.each([{}, { KUBB_OPENAPI_PATH: '' }, { KUBB_OPENAPI_PATH: '   ' }])(
        'rejects a missing temporary spec path',
        (env) => {
            expect(() => requireOpenApiInputPath(env)).toThrow(
                'KUBB_OPENAPI_PATH is required',
            )
        },
    )
})
