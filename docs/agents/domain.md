# Domain Docs

This repo uses the default single-context layout for agent skills.

| Source | Purpose |
| --- | --- |
| `AGENTS.md` | Primary repo instructions, workflow rules, and project-specific constraints. |
| `CONTEXT.md` | Optional domain language and product model when the repo needs a fuller glossary. |
| `docs/adr/` | Optional architecture decisions. Read relevant ADRs before architecture, debugging, TDD, or refactor work. |
| `docs/` | Product, architecture, release, and workflow documentation. Prefer existing docs before inventing new rules. |

If `CONTEXT.md` or `docs/adr/` does not exist, proceed from `AGENTS.md` and the existing docs. Do not create a context file only to satisfy a skill unless the missing domain vocabulary is actively slowing the work.
