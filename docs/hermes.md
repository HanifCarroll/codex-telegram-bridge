# Hermes Setup

This guide is the Hermes-first path for controlling local Codex threads through `codex-hermes-bridge`.

The product has two required lanes:

- Notification lane: Codex changes are pushed into Hermes without Hermes asking first.
- Control lane: Hermes responds to Codex through MCP tools such as `codex_reply` and `codex_approve`.

## Install

From a local checkout:

```bash
cargo install --path .
```

From Git:

```bash
cargo install --git https://github.com/hanifcarroll/codex-hermes-bridge
```

Verify the bridge can resolve Codex:

```bash
codex-hermes-bridge doctor
```

## Register With Hermes

If your default `hermes` command targets the profile you use, register MCP first:

```bash
codex-hermes-bridge hermes install
```

For the full product flow, add a Hermes webhook delivery target. The installer registers MCP, subscribes a Hermes webhook, and writes the daemon config that lets Codex notify Hermes without Hermes asking first:

```bash
codex-hermes-bridge hermes install \
  --webhook-deliver telegram \
  --webhook-deliver-chat-id <chat-id>
```

If you use a profile wrapper, pass it explicitly:

```bash
codex-hermes-bridge hermes install \
  --hermes-command hermes-se \
  --webhook-deliver telegram \
  --webhook-deliver-chat-id <chat-id>
```

Inspect the command without changing Hermes config:

```bash
codex-hermes-bridge hermes install --dry-run
```

The installer delegates to Hermes' own MCP config command:

```bash
hermes mcp add codex --command codex-hermes-bridge --args mcp
```

When `--webhook-deliver` is present, it also delegates to Hermes' webhook subscription command:

```bash
hermes webhook subscribe codex-watch \
  --events thread_waiting,thread_completed,thread_status_changed \
  --deliver telegram \
  --deliver-chat-id <chat-id> \
  --prompt '<Codex notification prompt>'
```

Restart Hermes after registration. Hermes discovers MCP tools when the profile starts. Then install and start the bridge daemon:

```bash
codex-hermes-bridge daemon install --dry-run
codex-hermes-bridge daemon install
codex-hermes-bridge daemon start
codex-hermes-bridge daemon status
```

The daemon reads `~/.codex-hermes-bridge/config.json`, watches Codex state, persists outbound notifications in SQLite, and retries delivery to Hermes until the signed webhook succeeds.

## Manual Config

You can also edit the Hermes profile config directly:

```yaml
mcp_servers:
  codex:
    command: "codex-hermes-bridge"
    args: ["mcp"]
    connect_timeout: 60
    timeout: 180
    sampling:
      enabled: false
```

Use the profile config for the agent you actually run. For example, a profile wrapper may set `HERMES_HOME=$HOME/.hermes/profiles/software-engineering`.

## Verify

List configured MCP servers:

```bash
hermes mcp list
```

Test the bridge server:

```bash
hermes mcp test codex
```

Hermes tool names are usually prefixed by the MCP server name. With server name `codex`, expect tools such as:

- `mcp_codex_codex_doctor`
- `mcp_codex_codex_threads`
- `mcp_codex_codex_inbox`
- `mcp_codex_codex_reply`
- `mcp_codex_codex_approve`

Verify the notification lane with a dry run:

```bash
codex-hermes-bridge hermes install \
  --hermes-command hermes-se \
  --webhook-deliver telegram \
  --webhook-deliver-chat-id <chat-id> \
  --webhook-secret test-secret \
  --dry-run
```

The JSON result should include `notificationLane.configured: true`, `notificationLane.daemonCommand`, and a redacted `notificationLane.config.webhookSecret`.

## Tool Surface

The MCP server exposes these bridge tools:

- `codex_doctor`: inspect the local Codex executable.
- `codex_threads`: sync and list recent threads.
- `codex_inbox`: list actionable inbox rows.
- `codex_waiting`: list threads waiting on user input or approval.
- `codex_show`: read one thread with derived state.
- `codex_reply`: resume and reply to a thread.
- `codex_approve`: send `YES` or `NO` for an approval prompt.

`watch`, `new`, `fork`, `archive`, `unarchive`, `away`, and `notify-away` are not exposed as default MCP tools. The OSS path keeps MCP focused on bounded control actions while the daemon owns proactive delivery.

## Resources

The MCP server exposes bounded JSON resources for clients that prefer context reads over tool calls:

- `codex://threads`: recent thread summaries.
- `codex://inbox`: actionable inbox rows.
- `codex://waiting`: threads waiting on user input or approval.
- `codex://thread/{threadId}`: one thread by id, exposed through a resource template.

These resources still read local Codex state. Treat them with the same trust boundary as tools.

## Prompts

The MCP server exposes small user-controlled prompts:

- `codex_check_inbox`: inspect Codex state before taking action.
- `codex_reply`: prepare a reply action for a specific thread and exact message.
- `codex_approve`: prepare an approval or denial action for a specific thread.

Prompts do not mutate Codex. They give Hermes safer phrasing for when and how to call the tools.

## Notification Lane

MCP is the control lane: Hermes asks for a bounded action, the bridge returns structured JSON.

`daemon run` is the notification lane: the bridge stays running, watches Codex state, queues outbound events durably, and posts them to Hermes through signed local webhooks.

Example:

```bash
codex-hermes-bridge daemon run --once
codex-hermes-bridge daemon run
```

`hermes post-webhook` remains available as the low-level signed HTTP poster. If `--secret` is omitted, it reads the secret from the daemon config written by `hermes install --webhook-deliver ...`.

The Hermes webhook payload includes:

- the original Codex watch event
- `event_type`
- `thread_id`
- `reply_route`, when a thread id is available
- `reply_route_marker`, retained only as a backward-compatible field for older Hermes builds

Hermes should persist `reply_route` against the delivered Telegram message. When the user replies directly to that Telegram message, Hermes should route the reply text back to the same Codex thread through `codex_reply` or the bridge CLI:

```bash
codex-hermes-bridge reply <threadId> --message "<telegram reply text>"
```

Do not depend on the model echoing `THREAD_ROUTE` text in its notification. That was useful for early local experiments, but the release contract is structured route metadata tied to the delivered platform message.

If you do not want Hermes webhook delivery, `watch --exec` can still target another trusted local notifier. Keep scripts outside the public repo when they contain personal chat ids, tokens, or local routing logic.

## Suggested Hermes Instruction

If your Hermes setup supports custom profile instructions or skills, add a small rule like this:

```text
When the user asks about Codex work, use the Codex MCP tools first. Check codex_inbox or codex_waiting before replying or approving. Use codex_reply only when the user gives the exact reply text. Use codex_approve only when the user explicitly approves or denies a prompt.
```

## Security Notes

- This is a local trust boundary. Do not expose `codex-hermes-bridge mcp` on a remote transport.
- MCP tools can read thread content and perform local Codex actions.
- The notification lane posts signed events to a local Hermes webhook. The daemon config stores the webhook secret locally; do not commit `~/.codex-hermes-bridge/config.json`.
- Keep `sampling.enabled: false` unless you have a specific reason to let this server request model sampling from the client.
- Use `dryRun: true` for reply and approve checks when you want Hermes to show the action shape before mutating Codex.
