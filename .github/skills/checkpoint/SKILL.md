---
name: checkpoint
description: "Save a compact progress snapshot so you can /compact or start a fresh chat without losing the thread."
user-invocable: true
disable-model-invocation: true
---
Write a checkpoint of the current work to `/memories/repo/progress.md` with the memory tool. Replace the file if it exists. Keep it to 40 lines or fewer and use only facts, not narrative:

```
# Progress — <task name> (<YYYY-MM-DD>)
GOAL: <one line>
DONE: <bullets: what changed, with file paths>
DECISIONS: <bullets: choices made + one-line why (include review verdicts)>
NEXT: <ordered bullets: exact next steps>
OPEN: <bullets: unresolved questions, failing tests, known bugs>
VERIFY: <commands/URLs that prove it works>
```

Also add any durable gotchas you discovered to `/memories/repo/facts.md`, one line each. Skip any that are already recorded there.

Then reply in one line: "Checkpoint saved. Run `/compact` to continue here, or open a new chat and say `resume`."
