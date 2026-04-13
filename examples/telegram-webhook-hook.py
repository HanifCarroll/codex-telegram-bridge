#!/usr/bin/env python3
import json
import os
import sys
import urllib.parse
import urllib.request


def main() -> int:
    payload = json.load(sys.stdin)
    if payload.get("type") != "thread_waiting":
        return 0

    thread = payload.get("thread") or {}
    prompt = thread.get("pendingPrompt") or {}
    if prompt.get("promptKind") not in {"reply", "approval"}:
        return 0

    token = os.environ.get("TELEGRAM_BOT_TOKEN")
    chat_id = os.environ.get("TELEGRAM_CHAT_ID")
    if not token or not chat_id:
        print("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID are required", file=sys.stderr)
        return 1

    name = thread.get("name") or payload.get("threadId") or "unknown-thread"
    question = prompt.get("question") or thread.get("lastPreview") or "Codex needs attention"
    prompt_kind = prompt.get("promptKind") or "attention"

    message = (
        f"Codex needs {prompt_kind}\n"
        f"Thread: {name}\n"
        f"Question: {question}\n"
        f"Thread ID: {payload.get('threadId')}"
    )

    body = urllib.parse.urlencode({
        "chat_id": chat_id,
        "text": message,
    }).encode("utf-8")
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
