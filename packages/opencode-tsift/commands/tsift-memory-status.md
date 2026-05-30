<!-- tsift:opencode-command v=0.1.62 name=tsift-memory-status -->
---
description: Inspect first-party tsift memory readiness
---

Run `tsift memory status <target> --json`, where `<target>` is `$ARGUMENTS` or `.` when no argument is provided. Summarize schema initialization, agent-doc hook contract, claude-mem import readiness, and the next bounded memory command to run. Do not import data unless the user explicitly asks for `--apply`.
