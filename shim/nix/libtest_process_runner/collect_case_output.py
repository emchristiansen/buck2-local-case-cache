"""Read one test process's output until it exits, times out, or leaves the pipe held open."""

import contextlib as _contextlib
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
    # The collector owns the launched case from its first step: whatever fails, during
    # setup or while reading, its group is killed and its leader reaped before the
    # failure is raised, and every descriptor taken so far is released.
    descriptors = _contextlib.ExitStack()
    chunks: list[bytes] = []
    exited_at = None
    at_eof = timed_out = left_output_open = False
    try:
        output = process.stdout
        if output is None:
            raise ValueError("a test process must be started with its output on a pipe")
        descriptors.callback(output.close)
        pipe = output.fileno()
        _os.set_blocking(pipe, False)
        exit_descriptor = _os.pidfd_open(process.pid)
        descriptors.callback(_os.close, exit_descriptor)
        selector = descriptors.enter_context(_selectors.DefaultSelector())
        selector.register(pipe, _selectors.EVENT_READ)
        selector.register(exit_descriptor, _selectors.EVENT_READ)
        deadline = None if timeout_seconds is None else _time.monotonic() + timeout_seconds
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
    except BaseException as error:
        # The original failure is what the caller sees; a failure ending the case is
        # recorded on it rather than replacing it.
        try:
            _end_case(process, groups, descriptors)
        except BaseException as failure:
            error.add_note(f"ending its case also failed: {failure!r}")
        raise
    returncode = _end_case(process, groups, descriptors)
    stopped_by = groups.stop_signal() if returncode < 0 and not timed_out else None
    return _CollectedCase(returncode, b"".join(chunks), timed_out, left_output_open, stopped_by)


def _end_case(
    process: _subprocess.Popen, groups: _LiveCaseGroups, descriptors: _contextlib.ExitStack
) -> int:
    # Before the test is reaped: until then its group id cannot be reused, so neither
    # a signal from `stop_all` nor `finish`'s kill of the group's survivors can reach
    # a recycled group. The descriptors are released whether or not that kill succeeds;
    # the leader is waited only once its group is killed, so a wait cannot hang on it.
    try:
        groups.finish(process)
    finally:
        descriptors.close()
    return process.wait()
