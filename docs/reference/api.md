# REST API

The backend's REST API is annotated with [`utoipa`](https://github.com/juhaku/utoipa) and an OpenAPI specification is generated from the route registration.

## No runtime docs endpoints

The server does **not** serve interactive API documentation or a raw spec at runtime: the Swagger UI / ReDoc / RapiDoc / Scalar endpoints and `/openapi.json` have been removed. They were unauthenticated and, on a public self-hosted deployment, would only expose the API surface to anyone — while the frontend client is generated **offline** (see below), so a runtime spec served no purpose.

To inspect the spec, generate it locally with the offline `dump-openapi` subcommand:

```bash
cargo run -p lcxl-remote-desk-server -- dump-openapi --out openapi.json
```

## Regenerating the Frontend Client

The frontend client (`vite-project/src/services/`) is generated from the OpenAPI spec with [Kubb](https://kubb.dev/). After changing the backend API, regenerate it (offline dump — no running server required):

```bash
cd vite-project
npm ci        # installs the exact Kubb version the lockfile pins
# Windows:
.\update_openapi.ps1
# Linux/macOS:
./update_openapi.sh
```

The scripts use the `dump-openapi` subcommand to export the spec from the route registration offline — no DB / Redis / HTTP needed. The spec is passed to Kubb through a temporary file and deleted afterward; generated `openapi.json` files are not tracked in the repository.

Kubb is pinned to an exact version and invoked as `npx --no-install`, so the generator can neither drift across patch releases nor be silently downloaded when dependencies are missing — the committed client stays reproducible from the lockfile. Run `npm ci` first if the regeneration fails to find Kubb.

::: tip
Generated files under `vite-project/src/services/` are produced by Kubb — do not edit them by hand.
:::

::: warning Regeneration is not optional
`npm run build` only runs tsc and vite; it never regenerates the client. A backend change that alters the spec therefore leaves a stale client that still compiles — and a changed numeric value produces no error at all, it just keeps being sent. CI regenerates on every push and fails if the result differs from what is committed.
:::

## Authentication contract

Browser/controller authentication uses one canonical JSON surface:

- `POST /api/auth/login`
- `POST /api/auth/logout`
- `GET /api/auth/me`
- `PATCH /api/auth/credentials`
- `POST /api/auth/tauri-login` (standalone desktop host only)

Every response uses `RestResponse`. Credential/business failures from login and
credentials remain HTTP 200 with `success=false`; an absent or expired session
on `/api/auth/me` is HTTP 401 with the same JSON envelope. Public fields use
snake_case. OAuth authorization/callback continuation remains under
`/api/oauth/*` and is not part of this route family.

## Error Codes

`DeskErrorCode` (`utils/src/error.rs`) is declared through the `desk_error_codes!` macro, which emits both the constants and an `ALL` name/value table. That table is published in the spec as an int32 enum carrying `x-enum-varnames`, so the generated client exposes `deskErrorCodeEnum` with named members — frontends branch on those instead of mirroring the numbers by hand.

Nothing references the type in a request or response body (`RestResponse.code` is a bare integer on the wire), so it reaches the spec only through the explicit registration in `server/src/openapi.rs`. Adding a code means adding one line to the macro and regenerating.

The frontend maps codes to text through `src/lib/desk-error-i18n.ts`, where each area keeps a small table of the codes it can receive. The fallback for an unmapped code is the caller's choice: show the backend `message`, or show a localized generic line. A `verify-error-codes` check runs before every build and rejects a code written as a bare number, so the generated constants stay the only source.
