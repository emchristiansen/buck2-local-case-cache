"""Read one test process's output until it exits, times out, or leaves the pipe held open."""

import os as _os
import selectors as _selectors
import signal as _signal
import subprocess as _subprocess
import time as _time

from libtest_process_runner.collected_case import CollectedCase as _CollectedCase
from libtest_process_runner.constants import LEAK_TIMEOUT_SECONDS as _LEAK_TIMEOUT
from libtest_process_runner.live_case_groups import LiveCaseGroups as _LiveCaseGroups


def collect_case_output(
    process: _subprocess.Popen, timeout_seconds: float | None, groups: _LiveCaseGroups
) -> _CollectedCase:
    output = process.stdout
    if output is None:
        raise ValueError("a test process must be started with its output on a pipe")
    pipe = output.fileno()
    _os.set_blocking(pipe, False)
    exit_descriptor = _os.pidfd_open(process.pid)
    selector = _selectors.DefaultSelector()
    selector.register(pipe, _selectors.EVENT_READ)
    selector.register(exit_descriptor, _selectors.EVENT_READ)
    deadline = None if timeout_seconds is None else _time.monotonic() + timeout_seconds
    chunks: list[bytes] = []
    exited_at = None
    at_eof = timed_out = left_output_open = False
    try:
        while exited_at is None or not at_eof:
            now = _time.monotonic()
            if exited_at is not None:
                wait = exited_at + _LEAK_TIMEOUT - now
                if wait <= 0:
                    left_output_open = True
                    break
            elif deadline is not None and not timed_out:
                wait = deadline - now
                if wait <= 0:
                    _os.killpg(process.pid, _signal.SIGKILL)
                    timed_out = True
                    continue
            else:
                wait = None
            for key, _events in selector.select(wait):
                if key.fd == exit_descriptor:
                    exited_at = _time.monotonic()
                    selector.unregister(exit_descriptor)
                    continue
                data = _os.read(pipe, 65536)
                if data:
                    chunks.append(data)
                else:
                    at_eof = True
                    selector.unregister(pipe)
    finally:
        # Before the test is reaped: until then its group id cannot be reused, so a
        # signal from `stop_all` cannot reach a recycled group.
        groups.finish(process)
        selector.close()
        _os.close(exit_descriptor)
        output.close()
    returncode = process.wait()
    stopped_by = groups.stop_signal() if returncode < 0 and not timed_out else None
    return _CollectedCase(returncode, b"".join(chunks), timed_out, left_output_open, stopped_by)
