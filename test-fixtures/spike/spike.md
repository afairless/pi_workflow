# Spike fixture plan — pi-plan dialog forwarding path

This inner repo is a self-contained fixture that manually proves pi-plan's
**real** dialog-answering path (Step 9 E2E gate of
`docs/research/plan-rust-orchestrator.md`): a worker's ask-gated op must
produce an inline `extension_ui_request` dialog that the operator answers and
that pi-plan forwards over the RPC extension-UI sub-protocol.

The gating facts (verified against the installed pi 0.85.1 and its
permission-system defaults):

- With no `pi-permissions.jsonc` present, the permission system's universal
  fallback is **`ask`** (`@gotgenes/pi-permission-system`
  `DEFAULT_UNIVERSAL_FALLBACK`), so an un-allowlisted `bash` command
  (`mkdir -p out`) and the `write` tool both resolve to `ask`.
- In `--mode rpc`, `ctx.hasUI = true`, so an `ask` surfaces as
  `extension_ui_request` on the worker's stdout, and pi-plan answers it with
  `extension_ui_response` on stdin (decision D9).
- `printf *` is allowlisted in the example ruleset and must NOT be the gated
  op.

So row 1's work requires both `mkdir` (gated bash) and the `write` tool
(gated tool): the spike cannot pass silently — no gated op, no dialog, no
approval, no commit.

Procedure and expected observations are in `README.md`; the operator-facing
checklist is `../../docs/acceptance-e2e.md`.
