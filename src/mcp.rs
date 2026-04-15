use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::Command;

use crate::codex::{
    attach_follow_result, build_show_thread_result, classify_app_server_error_message,
    normalized_message, parse_event_filter, resolve_codex_binary, sync_state_from_live,
    text_input_value, CodexAppServerClient, FollowRun,
};
use crate::state::{
    create_state_db, list_inbox_from_db, list_waiting_from_db, record_action, state_db_path,
};
use crate::{now_millis, ApproveResult, DoctorCodex, DoctorEnvelope, ReplyResult};

pub(crate) const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

pub(crate) fn run_mcp_server<R: BufRead, W: Write>(reader: R, mut writer: W) -> Result<()> {
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed = match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) => parsed,
            Err(error) => {
                let response =
                    mcp_error_response(None, -32700, format!("Parse error: {error}"), Value::Null);
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
                continue;
            }
        };

        if let Some(response) = mcp_handle_message(parsed) {
            writeln!(writer, "{}", serde_json::to_string(&response)?)?;
            writer.flush()?;
        }
    }
    Ok(())
}

pub(crate) fn mcp_handle_message(message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = match message.get("method").and_then(Value::as_str) {
        Some(method) => method,
        None => {
            return id.map(|id| {
                mcp_error_response(
                    Some(id),
                    -32600,
                    "Invalid request: missing method",
                    Value::Null,
                )
            });
        }
    };
    let params = message.get("params").unwrap_or(&Value::Null);

    match method {
        "notifications/initialized" => None,
        "initialize" => id.map(|id| mcp_success_response(id, mcp_initialize_result(params))),
        "ping" => id.map(|id| mcp_success_response(id, json!({}))),
        "tools/list" => id.map(|id| mcp_success_response(id, json!({ "tools": mcp_tools() }))),
        "tools/call" => id.map(|id| {
            let result = match mcp_tool_call_result(params) {
                Ok(result) => result,
                Err(error) => mcp_tool_error_result(&error),
            };
            mcp_success_response(id, result)
        }),
        "resources/list" => {
            id.map(|id| mcp_success_response(id, json!({ "resources": mcp_resources() })))
        }
        "resources/templates/list" => id.map(|id| {
            mcp_success_response(
                id,
                json!({
                    "resourceTemplates": [{
                        "uriTemplate": "codex://thread/{threadId}",
                        "name": "Codex Thread",
                        "title": "Codex Thread",
                        "description": "Read one Codex thread by id.",
                        "mimeType": "application/json"
                    }]
                }),
            )
        }),
        "resources/read" => id.map(|id| match mcp_read_resource(params) {
            Ok(result) => mcp_success_response(id, result),
            Err(error) => mcp_error_response(
                Some(id),
                -32602,
                format!("Invalid resource read: {error:#}"),
                Value::Null,
            ),
        }),
        "prompts/list" => {
            id.map(|id| mcp_success_response(id, json!({ "prompts": mcp_prompts() })))
        }
        "prompts/get" => id.map(|id| match mcp_get_prompt(params) {
            Ok(result) => mcp_success_response(id, result),
            Err(error) => mcp_error_response(
                Some(id),
                -32602,
                format!("Invalid prompt request: {error:#}"),
                Value::Null,
            ),
        }),
        _ => id.map(|id| {
            mcp_error_response(
                Some(id),
                -32601,
                format!("Method not found: {method}"),
                Value::Null,
            )
        }),
    }
}

fn mcp_success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn mcp_error_response(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    data: Value,
) -> Value {
    let mut response = serde_json::Map::new();
    response.insert("jsonrpc".to_string(), json!("2.0"));
    response.insert("id".to_string(), id.unwrap_or(Value::Null));
    response.insert(
        "error".to_string(),
        json!({
            "code": code,
            "message": message.into(),
            "data": data
        }),
    );
    Value::Object(response)
}

fn mcp_initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSION);
    let protocol_version = if requested == MCP_PROTOCOL_VERSION {
        requested
    } else {
        MCP_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            },
            "resources": {
                "listChanged": false,
                "subscribe": false
            },
            "prompts": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": env!("CARGO_PKG_NAME"),
            "title": "Codex Telegram Bridge",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Use these tools to inspect and control local Codex threads. Do not expose this server to untrusted agents or remote users."
    })
}

fn mcp_tool_call_result(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call requires params.name")?;
    let empty_args = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty_args);
    let payload = execute_mcp_tool(name, arguments)?;
    mcp_tool_success_result(payload)
}

fn mcp_tool_success_result(payload: Value) -> Result<Value> {
    Ok(json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload)?
        }],
        "structuredContent": payload,
        "isError": false
    }))
}

fn mcp_tool_error_result(error: &anyhow::Error) -> Value {
    let message = format!("{error:#}");
    json!({
        "content": [{
            "type": "text",
            "text": message
        }],
        "structuredContent": {
            "ok": false,
            "error": {
                "code": "tool_error",
                "message": message,
                "classified": classify_app_server_error_message(&format!("{error:#}"))
            }
        },
        "isError": true
    })
}

fn execute_mcp_tool(name: &str, arguments: &Value) -> Result<Value> {
    match name {
        "codex_doctor" => {
            let resolved = resolve_codex_binary()?;
            let output = Command::new(&resolved.path)
                .arg("--version")
                .output()
                .with_context(|| {
                    format!("failed to execute {} --version", resolved.path.display())
                })?;
            if !output.status.success() {
                bail!(
                    "codex binary {} returned non-zero exit status for --version",
                    resolved.path.display()
                );
            }
            serde_json::to_value(DoctorEnvelope {
                ok: true,
                codex: DoctorCodex {
                    resolved_path: resolved.path.display().to_string(),
                    source: resolved.source.to_string(),
                    version_stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                },
                bridge: crate::doctor_bridge()?,
            })
            .map_err(Into::into)
        }
        "codex_threads" => {
            let limit = mcp_arg_u64(arguments, &["limit"], 25)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            let result = sync_state_from_live(&mut client, &conn, now, limit, false)?;
            Ok(json!({ "threads": result["threads"].clone() }))
        }
        "codex_waiting" => {
            let project = mcp_arg_string(arguments, &["project"])?;
            let limit = mcp_arg_u64(arguments, &["limit"], 25)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
            serde_json::to_value(list_waiting_from_db(&conn, project.as_deref(), limit)?)
                .map_err(Into::into)
        }
        "codex_inbox" => {
            let project = mcp_arg_string(arguments, &["project"])?;
            let status = mcp_arg_string(arguments, &["status"])?;
            let attention = mcp_arg_string(arguments, &["attention"])?;
            let waiting_on = mcp_arg_string(arguments, &["waitingOn", "waiting_on"])?;
            let limit = mcp_arg_u64(arguments, &["limit"], 25)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
            serde_json::to_value(list_inbox_from_db(
                &conn,
                now,
                project.as_deref(),
                status.as_deref(),
                attention.as_deref(),
                waiting_on.as_deref(),
                limit,
            )?)
            .map_err(Into::into)
        }
        "codex_show" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            let result = client.request(
                "thread/read",
                json!({
                    "threadId": thread_id,
                    "includeTurns": true
                }),
            )?;
            build_show_thread_result(Some(&conn), &thread_id, result)
        }
        "codex_reply" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let message = mcp_required_string(arguments, &["message", "prompt"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let follow = mcp_arg_bool(arguments, &["follow"], false)?;
            let stream = mcp_arg_bool(arguments, &["stream"], false)?;
            let duration = mcp_arg_u64(arguments, &["durationMs", "duration"], 3000)?;
            let poll_interval = mcp_arg_u64(arguments, &["pollIntervalMs", "poll_interval"], 1000)?;
            let events = mcp_arg_events(arguments, &["events"])?;
            let sent_at = now_millis()?;
            if dry_run {
                serde_json::to_value(ReplyResult {
                    ok: true,
                    action: "reply",
                    dry_run: true,
                    thread_id: &thread_id,
                    message: &message,
                    sent_at,
                })
                .map_err(Into::into)
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
                let started = client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [text_input_value(&message)]
                    }),
                )?;
                let db_path = state_db_path()?;
                let conn = create_state_db(&db_path)?;
                record_action(
                    &conn,
                    &thread_id,
                    "reply",
                    json!({
                        "message": message,
                        "resumed": resumed,
                        "started": started,
                        "sentAt": sent_at
                    }),
                    sent_at,
                )?;
                let result = json!({
                    "ok": true,
                    "action": "reply",
                    "threadId": thread_id,
                    "message": message,
                    "sentAt": sent_at,
                    "resumed": resumed,
                    "started": started
                });
                attach_mcp_follow_if_requested(
                    result,
                    &mut client,
                    follow,
                    stream,
                    duration,
                    poll_interval,
                    events.as_deref(),
                )
            }
        }
        "codex_approve" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let decision = mcp_required_string(arguments, &["decision"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let follow = mcp_arg_bool(arguments, &["follow"], false)?;
            let stream = mcp_arg_bool(arguments, &["stream"], false)?;
            let duration = mcp_arg_u64(arguments, &["durationMs", "duration"], 3000)?;
            let poll_interval = mcp_arg_u64(arguments, &["pollIntervalMs", "poll_interval"], 1000)?;
            let events = mcp_arg_events(arguments, &["events"])?;
            let normalized = decision.trim().to_lowercase();
            let sent_text = match normalized.as_str() {
                "approve" => "YES",
                "deny" => "NO",
                _ => bail!("Approval decision must be approve or deny"),
            };
            let sent_at = now_millis()?;
            if dry_run {
                serde_json::to_value(ApproveResult {
                    ok: true,
                    action: "approve",
                    dry_run: true,
                    thread_id: &thread_id,
                    decision: &normalized,
                    sent_text,
                    sent_at,
                })
                .map_err(Into::into)
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
                let started = client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [text_input_value(sent_text)]
                    }),
                )?;
                let db_path = state_db_path()?;
                let conn = create_state_db(&db_path)?;
                record_action(
                    &conn,
                    &thread_id,
                    "approve",
                    json!({
                        "decision": normalized,
                        "sentText": sent_text,
                        "resumed": resumed,
                        "started": started,
                        "sentAt": sent_at
                    }),
                    sent_at,
                )?;
                let result = json!({
                    "ok": true,
                    "action": "approve",
                    "threadId": thread_id,
                    "decision": normalized,
                    "sentText": sent_text,
                    "sentAt": sent_at,
                    "resumed": resumed,
                    "started": started
                });
                attach_mcp_follow_if_requested(
                    result,
                    &mut client,
                    follow,
                    stream,
                    duration,
                    poll_interval,
                    events.as_deref(),
                )
            }
        }
        _ => bail!("Unknown MCP tool: {name}"),
    }
}

fn attach_mcp_follow_if_requested(
    result: Value,
    client: &mut CodexAppServerClient,
    follow: bool,
    stream: bool,
    duration: u64,
    poll_interval: u64,
    events: Option<&str>,
) -> Result<Value> {
    if !follow {
        return Ok(result);
    }
    let db_path = state_db_path()?;
    let conn = create_state_db(&db_path)?;
    let filter = parse_event_filter(events);
    if let Some(thread_id) = result.get("threadId").and_then(Value::as_str) {
        attach_follow_result(
            result.clone(),
            client,
            &conn,
            FollowRun {
                thread_id,
                duration_ms: duration,
                poll_interval_ms: poll_interval,
                event_filter: filter.as_ref(),
                stream,
            },
        )
    } else {
        Ok(result)
    }
}

fn mcp_arg_value<'a>(arguments: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = arguments.as_object()?;
    keys.iter().find_map(|key| object.get(*key))
}

fn mcp_arg_string(arguments: &Value, keys: &[&str]) -> Result<Option<String>> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::String(value)) => Ok(normalized_message(Some(value))),
        Some(Value::Null) | None => Ok(None),
        Some(other) => bail!("argument {} must be a string, got {}", keys[0], other),
    }
}

fn mcp_required_string(arguments: &Value, keys: &[&str]) -> Result<String> {
    mcp_arg_string(arguments, keys)?.with_context(|| format!("argument {} is required", keys[0]))
}

fn mcp_arg_bool(arguments: &Value, keys: &[&str], default: bool) -> Result<bool> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::Null) | None => Ok(default),
        Some(other) => bail!("argument {} must be a boolean, got {}", keys[0], other),
    }
}

fn mcp_arg_u64(arguments: &Value, keys: &[&str], default: u64) -> Result<u64> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Number(value)) => value
            .as_u64()
            .with_context(|| format!("argument {} must be an unsigned integer", keys[0])),
        Some(Value::Null) | None => Ok(default),
        Some(other) => bail!(
            "argument {} must be an unsigned integer, got {}",
            keys[0],
            other
        ),
    }
}

fn mcp_arg_events(arguments: &Value, keys: &[&str]) -> Result<Option<String>> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Array(values)) => {
            let mut events = Vec::new();
            for value in values {
                let event = value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .with_context(|| format!("argument {} must contain only strings", keys[0]))?;
                events.push(event.to_string());
            }
            Ok((!events.is_empty()).then(|| events.join(",")))
        }
        Some(Value::String(value)) => Ok(normalized_message(Some(value))),
        Some(Value::Null) | None => Ok(None),
        Some(other) => bail!(
            "argument {} must be a string or string array, got {}",
            keys[0],
            other
        ),
    }
}

fn mcp_resources() -> Vec<Value> {
    vec![
        json!({
            "uri": "codex://threads",
            "name": "Codex Threads",
            "title": "Codex Threads",
            "description": "Recent Codex threads from the local bridge.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "codex://inbox",
            "name": "Codex Inbox",
            "title": "Codex Inbox",
            "description": "Actionable Codex inbox rows with suggested actions.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "codex://waiting",
            "name": "Codex Waiting",
            "title": "Codex Waiting Threads",
            "description": "Codex threads waiting on user input or approval.",
            "mimeType": "application/json"
        }),
    ]
}

fn mcp_read_resource(params: &Value) -> Result<Value> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .context("resources/read requires params.uri")?;
    let payload = match uri {
        "codex://threads" => execute_mcp_tool("codex_threads", &json!({ "limit": 25 }))?,
        "codex://inbox" => execute_mcp_tool("codex_inbox", &json!({ "limit": 25 }))?,
        "codex://waiting" => execute_mcp_tool("codex_waiting", &json!({ "limit": 25 }))?,
        _ if uri.starts_with("codex://thread/") => {
            let thread_id = uri.trim_start_matches("codex://thread/");
            if thread_id.trim().is_empty() {
                bail!("thread resource uri is missing a thread id");
            }
            execute_mcp_tool("codex_show", &json!({ "threadId": thread_id }))?
        }
        _ => bail!("unknown Codex resource uri: {uri}"),
    };

    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": serde_json::to_string_pretty(&payload)?
        }]
    }))
}

fn mcp_prompts() -> Vec<Value> {
    vec![
        json!({
            "name": "codex_check_inbox",
            "title": "Check Codex Inbox",
            "description": "Ask Hermes to inspect current Codex work before taking action.",
            "arguments": []
        }),
        json!({
            "name": "codex_reply",
            "title": "Reply To Codex",
            "description": "Prepare an explicit reply action for a Codex thread.",
            "arguments": [
                {
                    "name": "threadId",
                    "description": "Codex thread id to reply to.",
                    "required": true
                },
                {
                    "name": "message",
                    "description": "Exact reply text to send.",
                    "required": true
                }
            ]
        }),
        json!({
            "name": "codex_approve",
            "title": "Approve Codex Prompt",
            "description": "Prepare an explicit approve or deny action for a Codex prompt.",
            "arguments": [
                {
                    "name": "threadId",
                    "description": "Codex thread id with the approval prompt.",
                    "required": true
                },
                {
                    "name": "decision",
                    "description": "Approval decision: approve or deny.",
                    "required": true
                }
            ]
        }),
    ]
}

fn mcp_get_prompt(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("prompts/get requires params.name")?;
    let empty_args = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty_args);
    let text = match name {
        "codex_check_inbox" => {
            "Check Codex state before taking action. Start with codex_inbox or codex_waiting. Summarize what is waiting, which threads need the user, and which action you recommend next.".to_string()
        }
        "codex_reply" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let message = mcp_required_string(arguments, &["message", "prompt"])?;
            format!(
                "Use the codex_reply tool with threadId `{thread_id}` and message `{message}`. Do not change the message unless the user explicitly asks you to revise it."
            )
        }
        "codex_approve" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let decision = mcp_required_string(arguments, &["decision"])?;
            format!(
                "Use the codex_approve tool with threadId `{thread_id}` and decision `{decision}`. Only approve or deny when the user explicitly requested that decision."
            )
        }
        _ => bail!("unknown Codex prompt: {name}"),
    };

    Ok(json!({
        "description": match name {
            "codex_check_inbox" => "Inspect Codex inbox state",
            "codex_reply" => "Reply to a Codex thread",
            "codex_approve" => "Approve or deny a Codex prompt",
            _ => "Codex prompt"
        },
        "messages": [{
            "role": "user",
            "content": {
                "type": "text",
                "text": text
            }
        }]
    }))
}

fn mcp_tools() -> Vec<Value> {
    vec![
        mcp_tool(
            "codex_doctor",
            "Codex Doctor",
            "Inspect the local Codex executable that the bridge will control.",
            mcp_schema(json!({}), vec![]),
            mcp_annotations(true, false, true, false),
        ),
        mcp_tool(
            "codex_threads",
            "List Codex Threads",
            "Sync live Codex state and list recent threads.",
            mcp_schema(
                json!({
                    "limit": integer_property("Maximum number of recent threads to sync and return.")
                }),
                vec![],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_waiting",
            "List Waiting Threads",
            "List Codex threads that are waiting for user input or approval.",
            mcp_schema(
                json!({
                    "project": string_property("Optional project label to filter by."),
                    "limit": integer_property("Maximum number of waiting threads to return.")
                }),
                vec![],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_inbox",
            "List Codex Inbox",
            "List actionable Codex inbox rows with attention and suggested-action metadata.",
            mcp_schema(
                json!({
                    "project": string_property("Optional project label to filter by."),
                    "status": string_property("Optional status type to filter by."),
                    "attention": string_property("Optional attention reason to filter by."),
                    "waitingOn": string_property("Optional waiting owner filter, such as me, codex, or none."),
                    "limit": integer_property("Maximum number of inbox rows to return.")
                }),
                vec![],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_show",
            "Show Codex Thread",
            "Read one Codex thread with derived bridge state and recent local actions.",
            mcp_schema(
                json!({
                    "threadId": string_property("Codex thread id to read.")
                }),
                vec!["threadId"],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_reply",
            "Reply To Codex Thread",
            "Resume a waiting Codex thread and send a user reply.",
            action_schema(
                json!({
                    "threadId": string_property("Codex thread id to resume."),
                    "message": string_property("Reply text to send."),
                    "dryRun": bool_property("Return the action shape without contacting Codex.")
                }),
                vec!["threadId", "message"],
            ),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_approve",
            "Approve Codex Prompt",
            "Approve or deny a waiting Codex approval prompt by sending YES or NO.",
            action_schema(
                json!({
                    "threadId": string_property("Codex thread id to resume."),
                    "decision": enum_property("Approval decision.", vec!["approve", "deny"]),
                    "dryRun": bool_property("Return the action shape without contacting Codex.")
                }),
                vec!["threadId", "decision"],
            ),
            mcp_annotations(false, false, false, true),
        ),
    ]
}

fn mcp_tool(
    name: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    annotations: Value,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
        "annotations": annotations
    })
}

fn mcp_schema(properties: Value, required: Vec<&str>) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn action_schema(mut properties: Value, required: Vec<&str>) -> Value {
    if let Some(object) = properties.as_object_mut() {
        object.insert(
            "follow".to_string(),
            bool_property("Collect a short follow result after the action."),
        );
        object.insert(
            "stream".to_string(),
            bool_property("Include follow events in the returned action result."),
        );
        object.insert(
            "durationMs".to_string(),
            integer_property("Follow duration in milliseconds."),
        );
        object.insert(
            "pollIntervalMs".to_string(),
            integer_property("Follow poll interval in milliseconds."),
        );
        object.insert(
            "events".to_string(),
            json!({
                "oneOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ],
                "description": "Optional follow event filter."
            }),
        );
    }
    mcp_schema(properties, required)
}

fn mcp_annotations(
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
) -> Value {
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": open_world
    })
}

fn string_property(description: &str) -> Value {
    json!({
        "type": "string",
        "description": description
    })
}

fn bool_property(description: &str) -> Value {
    json!({
        "type": "boolean",
        "description": description
    })
}

fn integer_property(description: &str) -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "description": description
    })
}

fn enum_property(description: &str, values: Vec<&str>) -> Value {
    json!({
        "type": "string",
        "enum": values,
        "description": description
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn mcp_initialize_advertises_tools_capability() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "hermes-test", "version": "1.0.0" }
            }
        }))
        .expect("initialize response");

        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(
            response["result"]["capabilities"]["resources"]["listChanged"],
            false
        );
        assert_eq!(
            response["result"]["capabilities"]["prompts"]["listChanged"],
            false
        );
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            "codex-telegram-bridge"
        );
    }

    #[test]
    fn mcp_tools_list_exposes_codex_control_surface_without_watch() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .expect("tools/list response");
        let tools = response["result"]["tools"].as_array().expect("tools array");
        let names = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<BTreeSet<_>>();

        for expected in [
            "codex_doctor",
            "codex_threads",
            "codex_inbox",
            "codex_waiting",
            "codex_show",
            "codex_reply",
            "codex_approve",
        ] {
            assert!(names.contains(expected), "missing MCP tool {expected}");
        }
        for hidden in [
            "codex_watch",
            "codex_follow",
            "codex_sync",
            "codex_setup",
            "codex_daemon",
            "codex_telegram",
            "codex_new",
            "codex_fork",
            "codex_archive",
            "codex_unarchive",
            "codex_away",
            "codex_notify_away",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} should stay out of the default Hermes MCP surface"
            );
        }

        let doctor = tools
            .iter()
            .find(|tool| tool["name"] == "codex_doctor")
            .expect("doctor tool");
        assert_eq!(doctor["annotations"]["readOnlyHint"], true);

        let reply = tools
            .iter()
            .find(|tool| tool["name"] == "codex_reply")
            .expect("reply tool");
        assert_eq!(
            reply["inputSchema"]["required"],
            json!(["threadId", "message"])
        );
    }

    #[test]
    fn mcp_tool_call_returns_structured_dry_run_result() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "codex_reply",
                "arguments": {
                    "threadId": "thr_1",
                    "message": "ship it",
                    "dryRun": true
                }
            }
        }))
        .expect("tools/call response");

        let result = &response["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["action"], "reply");
        assert_eq!(result["structuredContent"]["dry_run"], true);
        assert_eq!(result["structuredContent"]["thread_id"], "thr_1");
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"action\": \"reply\""));
    }

    #[test]
    fn mcp_tool_errors_return_tool_result_errors() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "codex_reply",
                "arguments": {
                    "threadId": "thr_1",
                    "dryRun": true
                }
            }
        }))
        .expect("tools/call response");

        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("argument message is required"));
    }

    #[test]
    fn mcp_rejects_unlisted_control_tools() {
        for name in [
            "codex_new",
            "codex_fork",
            "codex_archive",
            "codex_unarchive",
            "codex_away",
            "codex_notify_away",
            "codex_watch",
            "codex_follow",
            "codex_sync",
            "codex_setup",
            "codex_daemon",
            "codex_telegram",
        ] {
            let response = mcp_handle_message(json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": {}
                }
            }))
            .expect("tools/call response");

            assert_eq!(response["result"]["isError"], true);
            assert!(response["result"]["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Unknown MCP tool"));
        }
    }

    #[test]
    fn mcp_stdio_server_handles_handshake_discovery_and_dry_run_call() {
        let input = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": MCP_PROTOCOL_VERSION }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "codex_approve",
                    "arguments": {
                        "threadId": "thr_2",
                        "decision": "approve",
                        "dryRun": true
                    }
                }
            }),
        ]
        .into_iter()
        .map(|value| serde_json::to_string(&value).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        let mut output = Vec::new();

        run_mcp_server(std::io::Cursor::new(input), &mut output).expect("stdio MCP run");

        let lines = String::from_utf8(output)
            .expect("utf8 output")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("json line"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0]["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert!(lines[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "codex_approve"));
        assert_eq!(lines[2]["result"]["structuredContent"]["action"], "approve");
        assert_eq!(lines[2]["result"]["structuredContent"]["sent_text"], "YES");
    }

    #[test]
    fn mcp_resources_list_exposes_thread_context_resources() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "resources/list",
            "params": {}
        }))
        .expect("resources/list response");

        let resources = response["result"]["resources"]
            .as_array()
            .expect("resources array");
        assert!(resources
            .iter()
            .any(|resource| resource["uri"] == "codex://inbox"));
        assert!(resources
            .iter()
            .any(|resource| resource["uri"] == "codex://waiting"));
    }

    #[test]
    fn mcp_prompts_list_and_get_codex_reply_prompt() {
        let list = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "prompts/list",
            "params": {}
        }))
        .expect("prompts/list response");
        assert!(list["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|prompt| prompt["name"] == "codex_reply"));

        let get = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "prompts/get",
            "params": {
                "name": "codex_reply",
                "arguments": {
                    "threadId": "thr_1",
                    "message": "Continue with tests."
                }
            }
        }))
        .expect("prompts/get response");
        let text = get["result"]["messages"][0]["content"]["text"]
            .as_str()
            .expect("prompt text");
        assert!(text.contains("codex_reply"));
        assert!(text.contains("thr_1"));
        assert!(text.contains("Continue with tests."));
    }
}
