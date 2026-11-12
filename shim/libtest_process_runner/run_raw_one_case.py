"""Run one exactly selected test as libtest runs it and hand on its own evidence unchanged.

The strict per-case route parses each case's output itself, so the runner must not stand
between that output and the parser: a report the runner rebuilt from a summary line would
name the requested test, not the one libtest named, and would drop the output libtest
printed. Here the test process writes straight to the runner's own standard output and
standard error, and the runner exits with the test's own status. The runner still prepares
the test's environment as it does for every test: project-relative variables made
absolute, cleared variables removed, any PATH prefix, the run's own TMPDIR and each
fresh directory. It still bounds the test by the per-test timeout and stops it when the
runner itself is signalled. What it adds is said only on standard error.
"""

import os as _os
import select as _select
import signal as _signal
import subprocess as _subprocess
import sys as _sys

from libtest_process_runner.constants import RAW_ONE_CASE_TIMEOUT_EXIT as _TIMEOUT_EXIT
from libtest_process_runner.constants import USAGE_EXIT as _USAGE_EXIT
from libtest_process_runner.fresh_dir_environment import (
    fresh_dir_environment as _fresh_dir_environment,
)
from libtest_process_runner.live_case_groups import LiveCaseGroups as _LiveCaseGroups
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions

_FORWARDED_SIGNALS = (_signal.SIGTERM, _signal.SIGINT, _signal.SIGHUP)


def _wait_for_exit(process: _subprocess.Popen, timeout_seconds: float | None) -> bool:
    # Whether the test exited in time. It is left unreaped, so its process group id
    # cannot be reused until the group has been stopped.
    exit_descriptor = _os.pidfd_open(process.pid)
    try:
        while True:
            try:
                ready, _, _ = _select.select([exit_descriptor], [], [], timeout_seconds)
            except InterruptedError:
                continue
            return bool(ready)
    finally:
        _os.close(exit_descriptor)


def run_raw_one_case(options: _RunnerOptions, environment: dict[bytes, bytes]) -> int:
    [name] = options.filters
    # Exactly the command the strict route runs when it runs the binary itself.
    command = [options.binary, "--exact", name]
    try:
        environment = _fresh_dir_environment(environment, options.fresh_dir_env)
    except OSError as error:
        print(f"error: {name}: not run: cannot make its fresh directory: {error}", file=_sys.stderr)
        return _USAGE_EXIT
    if options.clear_env or options.fresh_dir_env:
        absent = [n for n in options.clear_env if _os.fsencode(n) not in environment]
        fresh = [f"{n}={_os.fsdecode(environment[_os.fsencode(n)])}" for n in options.fresh_dir_env]
        print(f"{name}: cleared {absent}; fresh {fresh}", file=_sys.stderr, flush=True)
    groups = _LiveCaseGroups()
    for number in _FORWARDED_SIGNALS:
        _signal.signal(number, lambda received, _frame: groups.stop_all(received))
    _sys.stdout.flush()
    _sys.stderr.flush()
    process = groups.launch(
        lambda: _subprocess.Popen(
            command, env=environment, stdin=_subprocess.DEVNULL, process_group=0
        )
    )
    if isinstance(process, int):
        print(
            f"error: {name}: not run: the runner received {_signal.Signals(process).name}",
            file=_sys.stderr,
        )
        return 128 + process
    try:
        exited = _wait_for_exit(process, options.per_test_timeout_seconds)
        # Whatever the test left running in its group would hold the output open; the
        # test itself, exited or not, is stopped with it.
        _os.killpg(process.pid, _signal.SIGKILL)
    finally:
        groups.finish(process)
    returncode = process.wait()
    if not exited:
        print(
            f"error: {name}: timed out after {options.per_test_timeout_seconds} seconds "
            "and was killed",
            file=_sys.stderr,
        )
        return _TIMEOUT_EXIT
    stopped_by = groups.stop_signal()
    if stopped_by is not None:
        return 128 + stopped_by
    return returncode if returncode >= 0 else 128 - returncode
