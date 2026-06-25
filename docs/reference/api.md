# REST API

The backend's REST API is annotated with [`utoipa`](https://github.com/juhaku/utoipa) and exposes an OpenAPI specification. The interactive documentation is served by the running server — it is always in sync with the build.

## Interactive Docs (server running)

Once the server is running, browse the API at:

- **Swagger UI** — `http://localhost:8081/swagger-ui/`
- **ReDoc** — `http://localhost:8081/redoc`
- **RapiDoc** — `http://localhost:8081/rapidoc`
- **Scalar** — `http://localhost:8081/scalar`

The raw specification is available at `http://localhost:8081/openapi.json`.

## Regenerating the Frontend Client

The frontend client (`vite-project/src/services/`) is generated from the OpenAPI spec with [Kubb](https://kubb.dev/). After changing the backend API, regenerate it (offline dump — no running server required):

```bash
cd vite-project
# Windows:
.\update_openapi.ps1
# Linux/macOS:
./update_openapi.sh
```

The scripts use the `dump-openapi` subcommand to export the spec from the route registration offline — no DB / Redis / HTTP needed.

::: tip
Generated files under `vite-project/src/services/` are produced by Kubb — do not edit them by hand.
:::
