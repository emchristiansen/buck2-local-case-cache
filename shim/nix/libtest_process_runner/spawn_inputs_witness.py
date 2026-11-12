"""Write one selected case's final spawn inputs, bound to its outcome, as one ASCII stderr line.

The record states what the runner passed to the launch after every rewrite: the
requested argv (its first element is the executable as requested, before any PATH
search), the working directory the child inherited and the exact environment bytes,
in the order they reach the child's envp. It is not an observation of the child: an
image chosen by PATH, a shebang or a replaced file, and anything the test changes
later, are not seen. A case that was never started says so, with the reason, and
carries no environment. Every
byte value is base64, so a decoder that is not byte-exact cannot alter it and no raw
libtest summary can appear in it.
"""

import base64 as _base64
import json as _json
import os as _os

from libtest_process_runner.case_result import CaseResult as _CaseResult
from libtest_process_runner.stderr_line import write_stderr_line as _write_stderr_line

PREFIX = b"libtest-process-runner spawn-inputs v1 "


def _b64(data: bytes) -> str:
    return _base64.b64encode(data).decode("ascii")


def spawn_inputs_witness(
    result: _CaseResult,
    command: list[str],
    cwd: bytes,
    environment: dict[bytes, bytes] | None,
    pid: int | None,
) -> None:
    """Write the record for `result`; `pid` is None when the case was never started.

    A case never started delivered no environment, so its record carries none, whatever
    was prepared; its reason says why it did not start.
    """
    record = {
        "claim": "spawn_inputs",
        "case": result.name,
        "started": pid is not None,
        "pid": pid,
        "argv": [_b64(_os.fsencode(arg)) for arg in command],
        "cwd": _b64(cwd),
        "envp": None
        if environment is None or pid is None
        else _b64(b"".join(name + b"=" + value + b"\0" for name, value in environment.items())),
        "outcome": result.outcome.name,
        "reason": result.reason,
    }
    # Cases run on several threads; each record is one uninterrupted line, shared with the
    # runner's other stderr lines.
    _write_stderr_line(
        PREFIX + _json.dumps(record, sort_keys=True, separators=(",", ":")).encode("ascii")
    )
