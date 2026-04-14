# Contributing

Thanks for contributing to `codex-hermes-bridge`.

## Development workflow

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
cargo package --allow-dirty --target-dir target/package-check
```

Keep changes narrowly scoped. Prefer small, reviewable diffs over broad refactors.

## Scope

The current public surface is intentionally focused on:
- thread inspection (`threads`, `show`, `waiting`, `inbox`, `sync`)
- thread actions (`reply`, `approve`)
- event streaming (`follow`, `watch`)
- archive flows (`archive`, `unarchive`)
- away-mode summaries (`away`, `notify-away`)
- Hermes MCP integration (`mcp`, `hermes install`)

If you want to add new commands or broaden the product surface, open an issue first.

## Hooks and examples

Examples under `examples/` should stay generic and reusable. Avoid project-specific relay logic in public examples.

## Security and secrets

Never commit credentials, tokens, or local environment files. Keep examples environment-variable based.

## Testing

Before opening a PR, make sure:
- `cargo fmt --check` passes
- `cargo test` passes
- `cargo clippy --all-targets -- -D warnings` passes
- `cargo package --allow-dirty --target-dir target/package-check` verifies the release package
- docs/examples still match the CLI surface
- MCP tool schemas and Hermes setup docs still match the implemented tool surface
