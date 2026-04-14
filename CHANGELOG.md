# Changelog

All notable changes to `codex-hermes-bridge` will be documented here.

## 0.1.0 - Unreleased

Initial OSS release candidate.

- Inspect Codex threads with `threads`, `show`, `waiting`, `inbox`, and `sync`.
- Take thread actions with `new`, `fork`, `reply`, `approve`, `archive`, and `unarchive`.
- Stream normalized JSON events with `follow` and `watch`.
- Run trusted local hooks with `watch --exec`.
- Emit away-mode notification summaries with `away` and `notify-away`.
- Expose a stdio MCP server for Hermes with structured Codex control tools.
- Expose MCP resources and prompts for Codex thread context and safer Hermes workflows.
- Add a `hermes install` helper that registers the bridge through `hermes mcp add` and can subscribe a Hermes webhook notification route.
- Add `hermes post-webhook` so `watch --exec` can push signed Codex change events into Hermes proactively.
- Include generic hook examples for printing events and sending Telegram notifications.
