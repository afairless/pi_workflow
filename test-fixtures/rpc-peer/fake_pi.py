#!/usr/bin/env python3
"""Scripted fake `pi --mode rpc` peer for pi-plan integration tests.

Speaks the pi RPC JSONL protocol over stdin/stdout from an action script
(JSONL, one action per line, `#` comments allowed). Do not depend on a real
pi binary in tests: this peer is the offline protocol peer (pattern from
twaldin's rpc_agent.py).

Action grammar (JSON object, processed strictly in order):

  {"stdin-expect": <json>}      Next frame from stdin (draining frames that
                                matched no respond first) must EQUAL every
                                listed key of <json>; extra incoming keys are
                                ignored. Mismatch or EOF => exit 1.

  {"respond": {"to": <json>, "with": <json>, "delay-ms"?: N}}
                                Stays active until consumed. The next incoming
                                frame that matches every listed key of `to`
                                is consumed and `with` is emitted (after
                                delay-ms when set, from a background thread so
                                later actions keep processing). Non-matching
                                frames are held for later actions.
                                Token "{{id}}" in `with` is replaced by the
                                consumed frame's id.

  {"stdout-emit": <json>}       Write the frame to stdout immediately.
  {"raw-emit": "<bytes>"}       Write raw bytes as-is (for bad frames/CRLF).
  {"emit-bytes": N}             Write N 'x' bytes + newline (oversize test).
  {"sleep-ms": N}               Pause the main loop.
  {"end": true}                 Stop processing; exit 0 only when no held
                                frames remain, else exit 1.

Delayed emissions run on non-daemon timer threads, so a final flush always
lands before the process exits (or raises BrokenPipeError if the client is
already gone, which is ignored).
"""

import json
import sys
import threading
from typing import NoReturn, Optional


def deep_subset(expected: object, actual: object) -> bool:
    """True when every key of `expected` equals the same key in `actual`."""
    if isinstance(expected, dict) and isinstance(actual, dict):
        return all(k in actual and deep_subset(v, actual[k])
                   for k, v in expected.items())
    return expected == actual


def fail(msg: str) -> NoReturn:
    print(f"fake_pi: {msg}", file=sys.stderr)
    sys.exit(1)


def load_actions(path: str) -> list:
    try:
        with open(path, encoding="utf-8") as f:
            raw_lines = f.readlines()
    except OSError as err:
        fail(f"cannot read action script {path}: {err}")
    actions = []
    for line in raw_lines:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            actions.append(json.loads(line))
        except json.JSONDecodeError as err:
            fail(f"action script line is not JSON: {err}")
    return actions


def main() -> None:
    if len(sys.argv) != 2:
        fail(f"usage: {sys.argv[0]} <action-script.jsonl>")
    actions = load_actions(sys.argv[1])

    held: list = []
    out_lock = threading.Lock()

    def emit_raw(data: bytes) -> bool:
        with out_lock:
            try:
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
                return True
            except BrokenPipeError:
                return False

    def emit_frame(frame: dict) -> None:
        emit_raw((json.dumps(frame, ensure_ascii=False) + "\n").encode("utf-8"))

    def emit_delayed(delay_ms: int, frame: dict) -> None:
        if delay_ms:
            threading.Timer(delay_ms / 1000.0, lambda: emit_frame(frame)).start()
        else:
            emit_frame(frame)

    def read_frame() -> Optional[dict]:
        if held:
            return held.pop(0)
        line = sys.stdin.readline()
        if line == "":
            return None
        try:
            parsed = json.loads(line)
        except json.JSONDecodeError:
            fail(f"stdin frame is not JSON: {line.strip()!r}")
        if not isinstance(parsed, dict):
            fail(f"stdin frame is not an object: {line.strip()!r}")
        return parsed

    for action in actions:
        if "stdin-expect" in action:
            expected = action["stdin-expect"]
            frame = read_frame()
            if frame is None:
                fail(f"EOF while expecting {json.dumps(expected)}")
            if not deep_subset(expected, frame):
                fail("stdin mismatch\n"
                     f"  expected keys: {json.dumps(expected)}\n"
                     f"  got:           {json.dumps(frame)}")
        elif "respond" in action:
            spec = action["respond"]
            while True:
                frame = read_frame()
                if frame is None:
                    fail("EOF while waiting for a respond match for "
                         f"{json.dumps(spec['to'])}")
                if deep_subset(spec["to"], frame):
                    out = json.dumps(spec["with"], ensure_ascii=False)
                    incoming_id = frame.get("id")
                    if incoming_id is not None:
                        out = out.replace("{{id}}", str(incoming_id))
                    try:
                        emitted = json.loads(out)
                    except json.JSONDecodeError:
                        fail(f"'with' is not JSON after token substitution: {out!r}")
                    emit_delayed(spec.get("delay-ms", 0), emitted)
                    break
                held.append(frame)
        elif "stdout-emit" in action:
            emit_frame(action["stdout-emit"])
        elif "raw-emit" in action:
            if not emit_raw(action["raw-emit"].encode("utf-8")):
                sys.exit(0)
        elif "emit-bytes" in action:
            if not emit_raw(b"x" * action["emit-bytes"] + b"\n"):
                sys.exit(0)
        elif "sleep-ms" in action:
            threading.Event().wait(action["sleep-ms"] / 1000.0)
        elif "end" in action:
            break
        else:
            fail(f"unknown action {json.dumps(action)}")
    else:
        fail("action script reached its end without an 'end' action")

    if held:
        fail(f"{len(held)} unexpected stdin frame(s) at end: "
             + ", ".join(json.dumps(f) for f in held))
    # Non-daemon timer threads keep running until their delayed emits land
    # (or the client's pipe closes), then the interpreter exits cleanly.
    sys.exit(0)


if __name__ == "__main__":
    main()