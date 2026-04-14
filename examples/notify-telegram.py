#!/usr/bin/env python3
import json
import os
import sys
import urllib.parse
import urllib.request


INTERESTING_EVENTS = {"thread_waiting", "thread_completed", "item_completed"}


def read_event():
    raw = sys.stdin.read()
    if not raw.strip():
        print("Expected one JSON event on stdin", file=sys.stderr)
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError as error:
        print(f"Invalid JSON event on stdin: {error}", file=sys.stderr)
        return None


def main() -> int:
    payload = read_event()
    if payload is None:
        return 1

    event_type = payload.get("type")
    if event_type not in INTERESTING_EVENTS:
        return 0

    token = os.environ.get("TELEGRAM_BOT_TOKEN")
    chat_id = os.environ.get("TELEGRAM_CHAT_ID")
    if not token or not chat_id:
        print("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID are required", file=sys.stderr)
        return 1

    thread = payload.get("thread") or {}
    prompt = thread.get("pendingPrompt") or {}
    name = thread.get("name") or payload.get("threadId") or "unknown-thread"

    if event_type == "thread_waiting":
        if prompt.get("promptKind") not in {"reply", "approval"}:
            return 0
        prompt_kind = prompt.get("promptKind") or "attention"
        body_text = prompt.get("question") or thread.get("lastPreview") or "Codex needs attention"
        title = f"Codex needs {prompt_kind}"
        detail_label = "Question"
    else:
        body_text = thread.get("lastPreview") or "Codex finished work"
        title = "Codex thread completed"
        detail_label = "Summary"

    message = "\n".join(
        [
            title,
            f"Thread: {name}",
            f"{detail_label}: {body_text}",
            f"Thread ID: {payload.get('threadId')}",
        ]
    )

    body = urllib.parse.urlencode({"chat_id": chat_id, "text": message}).encode("utf-8")
    request = urllib.request.Request(
        f"https://api.telegram.org/bot{token}/sendMessage",
        data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
        method="POST",
    )

    with urllib.request.urlopen(request, timeout=15) as response:
        if response.status < 200 or response.status >= 300:
            print(f"Telegram send failed with HTTP {response.status}", file=sys.stderr)
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
