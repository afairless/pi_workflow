# Fake `pi --mode rpc` peer

`fake_pi.py` is a scripted JSONL peer used by the RPC integration tests
(`tests/rpc_fake_pi.rs`). It speaks the pi RPC framing over stdin/stdout and
never touches a real model or session.

Each test drives a case file in `cases/` — an action script understood by
`fake_pi.py` (see the grammar in the script's docstring). Actions are
processed strictly in order; `respond` actions stay active until consumed
and non-matching stdin frames are held, so reordering and delayed responses
are expressible.

Requires Python 3 only, no third-party packages.
