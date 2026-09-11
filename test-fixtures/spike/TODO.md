# Implementation Plan: pi-plan spike — rows 1–3

Source: `spike.md`

## Prerequisites

- The pi-plan binary is built (`cargo build` at the repo root), or install it
  per `README.md`.
- Pi session cwd is THIS directory (`test-fixtures/spike/`).
- The inner repo has its initial commit:
  `git init && git add . && git commit -m "chore: seed spike fixture"`.
- Clear any leftover run state for a clean run:
  `rm -f supervisor-state.json .pi-plan-stop` and
  `rm -rf .pi-plan .pi` (the `.gitignore` keeps these out of git).

## Steps

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `feat: add hello file` | Spike | `out/hello.txt` | manual |
| 2 | `docs: finish spike` | Docs | `README.md` | — |
| 3 | `feat: add bye file` | Spike | `out/bye.txt` | manual |
