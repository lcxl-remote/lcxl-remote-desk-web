# LCXL Remote Desk — Documentation Site

The documentation site for the open-source LCXL Remote Desk project, built with [VitePress](https://vitepress.dev/).

The site is bilingual: **English** is the default (`/`), **简体中文** lives under `/zh/`.

## Local Development

```bash
# from this directory (web/docs)
npm install
npm run docs:dev        # start the dev server with hot reload
```

Open the printed local URL (default `http://localhost:5173`).

## Build & Preview

```bash
npm run docs:build      # build static site to .vitepress/dist
npm run docs:preview    # locally preview the production build
```

## Structure

```
docs/
├─ .vitepress/config.ts   # site config, nav & sidebar (en + zh)
├─ index.md               # English home page
├─ guide/ features/ config/ security/ reference/   # English content
├─ zh/                    # Chinese mirror (same structure)
└─ public/                # static assets (favicon, images)
```

## Deployment (GitHub Pages)

Pushing to `main` triggers `.github/workflows/docs.yaml`, which builds the site and
publishes it to GitHub Pages.

> **Base path**: for a GitHub Pages *project* site
> (`https://<user>.github.io/<repo>/`), the workflow sets `DOCS_BASE=/<repo>/`.
> For a user/org page or a custom domain, keep the default `/`.

## Editing Tips

- Add a page → create the `.md` file under both the English and `zh/` trees, then add
  it to the matching sidebar array in `.vitepress/config.ts`.
- Use static, localized SVG diagrams under `public/architecture/` and reference them from Markdown.
- API docs are intentionally not hand-written; the REST API page explains how to
  generate the `utoipa` spec offline with the `dump-openapi` subcommand.
