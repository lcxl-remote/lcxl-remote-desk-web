# Signaling Authentication

The project has connection paths with **different authentication mechanisms** that must never be confused.

| Connection | Authentication |
|---|---|
| Desk Server → Local Signaling | `settings.system.local_signaling_token` (auto-generated, `default` mode only) |
| Desk Server → Remote Signaling | `?token=<settings.system.signaling_token>` in the WebSocket URL |
| Desk Server → Manager | `?token=<settings.system.manager_api_token>` in the WebSocket URL |
| Browser → Signaling / Manager | **No token parameter.** Uses Actix-Session cookie authentication only. |

## Notes

- **Browser connections do not carry a token.** They authenticate with the Actix-Session cookie. Route extractors must use `Option<web::Query<VersionInfo>>`, and manager signaling routes are excluded from the global Session middleware.
- The local signaling token is auto-generated and only used in `default` mode.
- Tokens for remote signaling and the manager are passed in the WebSocket URL query string.

These mechanisms are distinct on purpose; mixing them up is a security defect.
