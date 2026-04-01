# iris-browser

Standalone workspace for the Iris native browser shell.

It contains the Iris Tauri app under `apps/iris` and the vendored Rust
Hashtree crates required to build and test the embedded daemon without the main
`hashtree` monorepo.

## Layout

- `apps/iris` - Svelte frontend, Tauri shell, Playwright/e2e tests, and local release scripts
- `rust/crates/*` - vendored Hashtree Rust crates used by the embedded daemon

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
