# Spike fixture conventions — pi-plan worker recipes

This inner repo drives pi-plan's manual approval-forwarding E2E spike
(`docs/acceptance-e2e.md`). A worker (a vanilla `pi --mode rpc` process)
reads `TODO.md` and implements **one row per run**. Every change is committed
with the exact message from the `## Steps` table — pi-plan detects row
completion by matching that message in `git log` (git-keyed, never marker
claims).

You have no parent agent to call. When you are blocked and need a decision,
end your final message with `PI_WORKER_STATUS: ASK` and a
`QUESTION: <crisp question>` line; a human's answer is folded into a fresh
worker that continues the row.

## Row 1 — `feat: add hello file` (deliverable: `out/hello.txt`)

Follow this recipe exactly — it is the dialog-forwarding proof:

1. Create the directory: `mkdir -p out`  (this `bash` is **gated** → an
   `extension_ui_request` dialog appears; answer inline to approve it).
2. Create the file with the **`write` tool** (also gated → a second dialog,
   approve it), content: `hello from the pi-plan spike`.
3. `git add out/hello.txt` and commit with exactly: `feat: add hello file`.
4. Finish your turn with `PI_WORKER_STATUS: COMPLETE`.

Do NOT `printf` the file from bash — `printf` is allowlisted and would skip
the gating proof.

## Row 2 — `docs: finish spike` (deliverable: `README.md`)

Append a short "Spike run notes" section to `README.md` recording what you
observed (dialogs forwarded and approved, commits landed, loop advance),
commit with exactly: `docs: finish spike`, and finish with
`PI_WORKER_STATUS: COMPLETE`.

## Row 3 — `feat: add bye file` (deliverable: `out/bye.txt`)

1. Create the file with the **`write` tool** (gated → approve the forwarded
   dialog), content: `bye from the pi-plan spike`.
2. `git add out/bye.txt` and commit with exactly: `feat: add bye file`.
3. Finish your turn with `PI_WORKER_STATUS: COMPLETE`.

When a decision is genuinely missing (the loop paused with a question for
you), do not speculate — end with `PI_WORKER_STATUS: ASK` and a crisp
`QUESTION:` line.
