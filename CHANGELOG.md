# Changelog

All notable changes to `codex-hermes-bridge` will be documented here.

## 0.1.0 - Unreleased

Initial OSS release candidate.

- Inspect Codex threads with `threads`, `show`, `waiting`, `inbox`, and `sync`.
- Take thread actions with `new`, `fork`, `reply`, `approve`, `archive`, and `unarchive`.
- Stream normalized JSON events with `follow` and `watch`.
- Run trusted local hooks with `watch --exec`.
- Configure the full product path with `setup`.
- Gate outbound Telegram notifications with `away on/off` so users are not notified while present.
- Send proactive Codex notifications directly through Telegram with `telegram setup`, `telegram test`, and the local daemon.
- Route Telegram reply-to-message text and approval buttons back to the originating Codex thread.
- Expose a stdio MCP server for Hermes with structured Codex control tools.
- Expose MCP resources and prompts for Codex thread context and safer Hermes workflows.
- Add a `hermes install` helper that registers the bridge through `hermes mcp add`.
- Prune legacy away-summary and hidden MCP control paths so the daemon and documented MCP tools are the only notification/control lanes.
- Hide advanced local sync, event-stream, and maintenance commands from default CLI help while keeping them available for automation.
- Keep hook examples generic and leave Telegram delivery to the bridge daemon.
