# Hermes Setup

This guide is the Hermes-first path for controlling local Codex threads through `codex-hermes-bridge`.

Use MCP for control. Use `watch --exec` for events and notifications.

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

If your default `hermes` command targets the profile you use:

```bash
codex-hermes-bridge hermes install
```

If you use a profile wrapper, pass it explicitly:

```bash
codex-hermes-bridge hermes install --hermes-command hermes-se
```

Inspect the command without changing Hermes config:

```bash
codex-hermes-bridge hermes install --dry-run
```

The installer delegates to Hermes' own MCP config command:

```bash
hermes mcp add codex --command codex-hermes-bridge --args mcp
```

Restart Hermes after registration. Hermes discovers MCP tools when the profile starts.

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

## Optional Daemon Lane

MCP is the control lane: Hermes asks for a bounded action, the bridge returns structured JSON.

`watch --exec` is the daemon lane: the bridge stays running, watches Codex state, and pushes each event to a trusted local hook.

Example:

```bash
codex-hermes-bridge watch \
  --events thread_waiting,thread_completed,thread_status_changed \
  --exec "python3 examples/print-hook-event.py"
```

Use this lane for Telegram, desktop notifications, Slack relays, or profile-specific Hermes delivery. Keep hook scripts outside the public repo when they contain personal chat ids, tokens, or local routing logic.

## Suggested Hermes Instruction

If your Hermes setup supports custom profile instructions or skills, add a small rule like this:

```text
When the user asks about Codex work, use the Codex MCP tools first. Check codex_inbox or codex_waiting before replying or approving. Use codex_reply only when the user gives the exact reply text. Use codex_approve only when the user explicitly approves or denies a prompt. Do not call archive tools without explicit confirmation unless dryRun is true.
```

## Security Notes

- This is a local trust boundary. Do not expose `codex-hermes-bridge mcp` on a remote transport.
- MCP tools can read thread content and perform local Codex actions.
- Keep `sampling.enabled: false` unless you have a specific reason to let this server request model sampling from the client.
- Use `dryRun: true` for archive, reply, approve, new, fork, and unarchive checks when you want Hermes to show the action shape before mutating Codex.
