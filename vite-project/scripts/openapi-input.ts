const OPENAPI_INPUT_ENV = 'KUBB_OPENAPI_PATH'

export function requireOpenApiInputPath(
    env: Readonly<Record<string, string | undefined>> = process.env,
): string {
    const inputPath = env[OPENAPI_INPUT_ENV]?.trim()
    if (!inputPath) {
        throw new Error(
            `${OPENAPI_INPUT_ENV} is required; run update_openapi.sh or update_openapi.ps1`,
        )
    }
    return inputPath
}
