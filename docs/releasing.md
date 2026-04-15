# Releasing

`codex-telegram-bridge` uses a tag-driven GitHub release flow.

## Before Tagging

Update [CHANGELOG.md](../CHANGELOG.md) so the release entry is dated and no longer marked `Unreleased`, then run:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo package --locked --target-dir target/package-check
```

## Publish A Release

From `main`, create and push an annotated tag:

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

Pushing the tag triggers `.github/workflows/release.yml`, which:

1. re-runs formatting, tests, clippy, and package verification
2. builds release archives for the GitHub-hosted Linux and macOS runners
3. creates or updates the matching GitHub release from the changelog entry
4. uploads the generated archives and `sha256` files to that release
5. optionally runs `cargo publish --locked` if `CARGO_REGISTRY_TOKEN` is configured in GitHub Actions secrets

## Notes

- GitHub release notes are sourced from the matching changelog section for the tag version.
- If the release workflow is re-run, it updates the existing release and re-uploads assets with `--clobber`.
- The preferred install paths for users remain `cargo install --git ...`, `cargo install --path .`, or downloading a release archive from GitHub Releases.
