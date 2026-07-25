# featherbit docs site

Docusaurus site: landing page + documentation for the featherbit gateway.

## Commands

```bash
npm install
npm run start      # dev server (no API docs sync)
npm run sync-api   # copy rustdoc (../target/doc) and TypeDoc (../ui/docs) into static/api/
npm run build      # sync-api + production build (fails on broken links)
npm run serve      # serve the production build locally
```

The API navbar links expect the generated references. Build them first:

```bash
cargo doc --no-deps --document-private-items   # repo root
cd ui && npm run docs                          # TypeDoc
```

## Deploying to GitHub Pages

`docusaurus.config.ts` is configured for `francescodedomenico.github.io/featherbit/`. The deploy workflow is `.github/workflows/docs.yml` — enable GitHub Pages with "GitHub Actions" as the source in the repo settings.
