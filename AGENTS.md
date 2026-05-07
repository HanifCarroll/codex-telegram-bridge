# Codex Telegram Bridge Agent Instructions

## SDLC Operating Model

Work tracking: GitHub Issues are canonical for repo-bound bugs, features, refactors, and release tasks. Local markdown is for scratch thinking before promotion.

- Use gstack as the default SDLC rail: `gstack-office-hours` for idea shaping, `gstack-plan-eng-review` or `gstack-autoplan` for plan review, `gstack-investigate` for debugging, `gstack-qa` or `gstack-qa-only` for QA, `gstack-review` for pre-landing review, `gstack-ship` for PR/ship flow, and `gstack-land-and-deploy` or `gstack-canary` for deploy verification.
- Use Matt Pocock skills selectively: `to-prd` for formal client/product PRDs, `to-issues` for splitting approved plans into issue-ready slices, `triage` for issue-state management, `tdd` for test-first implementation, `improve-codebase-architecture` for refactor discovery, and `grill-with-docs` for domain-aware plan pressure-testing.
- Treat Superpowers as explicit heavy mode for large or risky work that needs specs, worktrees, subagent execution, code-review gates, and verification discipline. Do not use it by default for one-off bugs, small UI tweaks, routine refactors, or urgent fixes.
- Every substantive Codex thread should have a named work item, desired outcome, acceptance criteria, risk notes, verification plan, and final status: done, blocked, parked, or split.
- Matt issue/PRD/triage skills should read `docs/agents/issue-tracker.md`, `docs/agents/triage-labels.md`, and `docs/agents/domain.md` before publishing or labeling work.
