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

For the full product flow, add a Hermes webhook delivery target. The installer registers MCP, subscribes a Hermes webhook, and prints the exact watcher command to run:

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

Restart Hermes after registration. Hermes discovers MCP tools when the profile starts. Then run the printed `notificationLane.watcherCommand` as a long-lived local process so Codex changes proactively reach Hermes.

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

The JSON result should include `notificationLane.configured: true` and a `notificationLane.watcherCommand` that calls `codex-hermes-bridge hermes post-webhook`.

## Tool Surface

The MCP server exposes these bridge tools:

- `codex_doctor`: inspect the local Codex executable.
- `codex_threads`: sync and list recent threads.
- `codex_inbox`: list actionable inbox rows.
- `codex_waiting`: list threads waiting on user input or approval.
- `codex_show`: read one thread with derived state.
- `codex_new`: start a new thread.
- `codex_fork`: fork a thread.
- `codex_reply`: resume and reply to a thread.
- `codex_approve`: send `YES` or `NO` for an approval prompt.
- `codex_archive`: archive explicit or filtered thread selections.
- `codex_unarchive`: unarchive one thread.
- `codex_away`: manage away-mode state.
- `codex_notify_away`: build away-mode notification summaries.

`watch` is not exposed as an MCP tool. It is a long-running stream, while MCP tools should return bounded request/response results.

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

`watch --exec` is the notification lane: the bridge stays running, watches Codex state, and pushes each event to a trusted local hook. For Hermes, the trusted hook is `hermes post-webhook`.

Example:

```bash
codex-hermes-bridge watch \
  --events thread_waiting,thread_completed,thread_status_changed \
  --exec 'codex-hermes-bridge hermes post-webhook --url http://localhost:8644/webhooks/codex-watch --secret <secret>'
```

`hermes post-webhook` signs the payload using the same `X-Hub-Signature-256` convention Hermes' webhook gateway validates. The payload includes:

- the original Codex watch event
- `event_type`
- `thread_id`
- `reply_route_marker`, when a thread id is available

The default Hermes webhook prompt tells Hermes to notify the user and include the reply route marker as the final line. Hermes can then use its platform reply routing and the MCP `codex_reply` or `codex_approve` tools to send the user's response back to Codex.

If you do not want Hermes webhook delivery, `watch --exec` can still target another trusted local notifier. Keep scripts outside the public repo when they contain personal chat ids, tokens, or local routing logic.

## Suggested Hermes Instruction

If your Hermes setup supports custom profile instructions or skills, add a small rule like this:

```text
When the user asks about Codex work, use the Codex MCP tools first. Check codex_inbox or codex_waiting before replying or approving. Use codex_reply only when the user gives the exact reply text. Use codex_approve only when the user explicitly approves or denies a prompt. Do not call archive tools without explicit confirmation unless dryRun is true.
```

## Security Notes

- This is a local trust boundary. Do not expose `codex-hermes-bridge mcp` on a remote transport.
- MCP tools can read thread content and perform local Codex actions.
- The notification lane posts signed events to a local Hermes webhook. Keep webhook secrets out of committed files and shell history where possible.
- Keep `sampling.enabled: false` unless you have a specific reason to let this server request model sampling from the client.
- Use `dryRun: true` for archive, reply, approve, new, fork, and unarchive checks when you want Hermes to show the action shape before mutating Codex.
