# iris-browser

Standalone workspace for the Iris native browser shell.

It contains the Iris Tauri app under `apps/iris`. The embedded daemon uses the
published Hashtree Rust crates through Cargo.

## Layout

- `apps/iris` - Svelte frontend, Tauri shell, Playwright/e2e tests, and local release scripts

## Development

```bash
pnpm install
pnpm build
pnpm test
pnpm run test:rust
```

Native smoke and release helpers run from this repo directly:

```bash
pnpm run test:native:docker
pnpm run release:native -- --dry-run --only linux
```

Git remote setup for Hashtree-first development:

```bash
git remote add origin htree://self/iris-browser
```
