#!/usr/bin/env python3
import json
import sys


def main() -> int:
    payload = json.load(sys.stdin)
    event_type = payload.get("type")
    if event_type not in {"thread_waiting", "thread_completed", "item_completed"}:
        return 0

    thread = payload.get("thread") or {}
    name = thread.get("name") or payload.get("threadId") or "unknown-thread"
    prompt = thread.get("pendingPrompt") or {}
    if event_type == "thread_waiting":
        if prompt.get("promptKind") not in {"reply", "approval"}:
            return 0
        message = prompt.get("question") or thread.get("lastPreview") or "Codex needs attention"
    else:
        message = thread.get("lastPreview") or "Codex finished work"

    # Replace this print with the real Hermes/OpenClaw relay step.
    print(json.dumps({
        "notify": True,
        "threadId": payload.get("threadId"),
        "eventType": event_type,
        "name": name,
        "promptKind": prompt.get("promptKind"),
        "message": message,
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
