#!/usr/bin/env python3
import json
import sys


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

    thread = payload.get("thread") or {}
    prompt = thread.get("pendingPrompt") or {}
    name = thread.get("name") or payload.get("threadId") or "unknown-thread"

    if event_type == "thread_waiting":
        if prompt.get("promptKind") not in {"reply", "approval"}:
            return 0
        message = prompt.get("question") or thread.get("lastPreview") or "Codex needs attention"
    else:
        message = thread.get("lastPreview") or "Codex finished work"

    print(
        json.dumps(
            {
                "notify": True,
                "threadId": payload.get("threadId"),
                "eventType": event_type,
                "name": name,
                "promptKind": prompt.get("promptKind"),
                "message": message,
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
