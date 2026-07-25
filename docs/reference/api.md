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
# Windows:
.\update_openapi.ps1
# Linux/macOS:
./update_openapi.sh
```

The scripts use the `dump-openapi` subcommand to export the spec from the route registration offline — no DB / Redis / HTTP needed. The spec is passed to Kubb through a temporary file and deleted afterward; generated `openapi.json` files are not tracked in the repository.

::: tip
Generated files under `vite-project/src/services/` are produced by Kubb — do not edit them by hand.
:::
