<!-- tsift:opencode-command v=0.1.62 name=tsift-memory-status -->
---
description: Inspect first-party tsift memory readiness
---

Run `tsift memory status <target> --json`, where `<target>` is `$ARGUMENTS` or `.` when no argument is provided. Summarize schema initialization, agent-doc hook contract, graph-db retrieval readiness, and claude-mem fallback import readiness. Do not import data unless the user explicitly asks for `--apply`.
