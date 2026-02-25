# Development Guide

[简体中文](DEVELOPMENT_CN.md)

This document provides a comprehensive guide for the LCXL Remote Desk Web project, including environment setup, development workflow, API documentation, and coding standards.

## Table of Contents

- [Requirements](#requirements)
- [Quick Start](#quick-start)
- [Configuration Details](#configuration-details)
- [API Documentation](#api-documentation)
- [Development Workflow](#development-workflow)
- [Coding Standards](#coding-standards)

## Requirements

### Rust Development

- Rust 1.90 or higher
- Cargo

### Frontend Development

- Node.js 12.0.0 or higher
- npm, yarn, or pnpm

### Linux System Dependencies

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev clang libclang-dev cmake libvpx-dev
```

## Quick Start

### 1. Backend Development

Configure `conf/config.toml` and run:

```bash
cargo run
```

### 2. Frontend Development

```bash
cd vite-project
npm install
npm run dev
```

## API Documentation

Once the server is running, access documentation at:

- **Swagger UI**: `http://localhost:8081/swagger-ui/`
- **ReDoc**: `http://localhost:8081/redoc`

## Development Workflow

### Project Structure

- `server/`: Main server application
- `signal/`: WebRTC signaling & TURN services
- `vite-project/`: React frontend
- `utils/`: Common utilities

### Adding Features

1. Define models in `server/src/model/`.
2. Implement logic in `server/src/service/`.
3. Add route handlers in `server/src/controller/`.
4. Register routes in `server/src/main.rs`.

## Coding Standards

- **Rust**: Follow `rustfmt` and run `cargo clippy`.
- **Frontend**: Follow ESLint and Prettier.

## Building and Deployment

### Production Build

```bash
# Backend
cargo build --release

# Frontend
cd vite-project
npm run build
```

### Docker

Use `./build_docker.sh` for easy building, or `docker-compose` for quick deployment.
