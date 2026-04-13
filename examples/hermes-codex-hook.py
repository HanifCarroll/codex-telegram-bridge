#!/usr/bin/env python3
import json
import sys


def main() -> int:
    payload = json.load(sys.stdin)
    if payload.get("type") != "thread_waiting":
        return 0

    thread = payload.get("thread") or {}
    prompt = thread.get("pendingPrompt") or {}
    if prompt.get("promptKind") not in {"reply", "approval"}:
        return 0

    name = thread.get("name") or payload.get("threadId") or "unknown-thread"
    question = prompt.get("question") or thread.get("lastPreview") or "Codex needs attention"

    # Replace this print with the real Hermes/OpenClaw relay step.
    print(json.dumps({
        "notify": True,
        "threadId": payload.get("threadId"),
        "name": name,
        "promptKind": prompt.get("promptKind"),
        "message": question,
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
