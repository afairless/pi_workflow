# pi-plan spike — manual run procedure

This fixture proves pi-plan's inline dialog-answering path (Step 9 E2E gate
in `docs/research/plan-rust-orchestrator.md`, §Step 9). It is a **manual**
gate: it must be run with the real `pi` binary, a real `pi --mode rpc`
worker, real permission dialogs answered inline, and real commits.

## Setup (one time)

```bash
cd test-fixtures/spike
git init
git add . && git commit -m "chore: seed spike fixture"
# optional: git config user.name/user.email if not already set
```

Any run state left behind by a previous run is ignored by git
(`supervisor-state.json`, `.pi-plan-stop`, `.pi-plan/`, `.pi/`); delete it to
start clean:

```bash
rm -f supervisor-state.json .pi-plan-stop
rm -rf .pi-plan .pi
```

## Procedure

1. At the repo root, build the binary: `cargo build`.
2. `cd test-fixtures/spike`.
3. Run `pi-plan supervise` (or `cargo run --manifest-path` style — the repo
   binary is `target/debug/pi-plan`; call it directly as `pi-plan` or via
   `../target/debug/pi-plan`).
4. **Expect a live tail** (stderr) as the row-1 worker streams events, plus a
   status line (`row 1 · agent … · turns … · ctx …`).
5. **Expect an inline dialog** (stdout) when the worker runs `mkdir -p out`
   (gated bash). Answer `y`/option to approve it.
6. **Expect a second inline dialog** for the `write` tool call. Approve it.
7. Watch the worker commit `feat: add hello file`; pi-plan advances the loop
   to row 2 and spawns a fresh worker.
8. Approve any further dialogs; row 2 commits `docs: finish spike`; row 3
   commits `feat: add bye file`.
9. The loop reports the plan complete and exits 0; `out/hello.txt` and
   `out/bye.txt` exist; the final report lists every row `done`.

## Pass criteria

- Row 1 **cannot** complete without both dialogs being approved (the work is
  gated by design). A run that completes row 1 without any dialog is
  INVALID — the gating assumption broke; stop and investigate.
- Both commits land with exactly the planned messages.
- The loop advances rows and reports completion.
- `pi-plan status` agrees with `git log --oneline` at every section break.

## Results

| Date | Operator | Result | Notes |
| --- | --- | --- | --- |
| — | — | ⏳ PENDING — run this spike in a real pi session | — |

## Spike run notes

Row 2's worker appends its observations here.
