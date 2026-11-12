"""Run one exactly selected test as libtest runs it and hand on its own evidence unchanged.

The strict per-case route parses each case's output itself, so the runner must not stand
between that output and the parser: a report the runner rebuilt from a summary line would
name the requested test, not the one libtest named, and would drop the output libtest
printed. Here the test process writes straight to the runner's own standard output and
standard error, and the runner exits with the test's own status. The runner still prepares
the test's environment as it does for every test: project-relative variables made
absolute, cleared variables removed, any PATH prefix, the run's own TMPDIR and each
fresh directory. It still bounds the test by the per-test timeout and stops it when the
runner itself is signalled. What it adds is said only on standard error. With
`--spawn-inputs-witness` the test's standard output passes through the runner unchanged,
byte for byte as it arrives, so the runner can read libtest's result line for the case and
write the case's witness after it (raw_one_case_witness, raw_one_case_relay). The test's
standard error stays its own, so the witness follows a newline the runner writes: the record
starts a line whatever the test's last byte was. A test that cannot be launched at all is
recorded as not started, with the reason, and the launch error is raised as on the External
route.
"""

import os as _os
import select as _select
import signal as _signal
import subprocess as _subprocess
import sys as _sys

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome
from libtest_process_runner.case_result import CaseResult as _CaseResult
from libtest_process_runner.constants import RAW_ONE_CASE_TIMEOUT_EXIT as _TIMEOUT_EXIT
from libtest_process_runner.constants import USAGE_EXIT as _USAGE_EXIT
from libtest_process_runner.fresh_dir_environment import (
    fresh_dir_environment as _fresh_dir_environment,
)
from libtest_process_runner.live_case_groups import LiveCaseGroups as _LiveCaseGroups
from libtest_process_runner.raw_one_case_relay import RawOneCaseRelay as _RawOneCaseRelay
from libtest_process_runner.raw_one_case_witness import raw_case_record as _raw_case_record
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.spawn_inputs_witness import (
    spawn_inputs_witness as _spawn_inputs_witness,
)

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
    seen = {"command": command, "cwd": _os.getcwdb(), "environment": None, "pid": None}
    try:
        status, reason, stdout = _run(options, environment, command, seen)
    except OSError as error:
        if not options.spawn_inputs_witness or "launching" not in seen or seen["pid"] is not None:
            raise
        refused = _CaseResult(name, _CaseOutcome.FAILED, f"not started: {error!r}", b"", False)
        _witness(refused, seen)
        raise
    if not options.spawn_inputs_witness:
        return status
    if seen["pid"] is None:
        # Never started: no environment was delivered, and the case fails as reported.
        result = _CaseResult(name, _CaseOutcome.FAILED, reason, b"", False)
    else:
        result = _raw_case_record(name, stdout, status, reason)
    if result is not None and not _witness(result, seen):
        # Missing evidence never passes for a recorded case.
        return status if status != 0 else _USAGE_EXIT
    return status


def _witness(result: _CaseResult, seen: dict) -> bool:
    # Whether the record was written, on a line of its own after the test's stderr.
    try:
        _sys.stderr.flush()
        _os.write(2, b"\n")
        _spawn_inputs_witness(
            result, seen["command"], seen["cwd"], seen["environment"], seen["pid"]
        )
    except Exception as error:
        message = f"error: {result.name}: its spawn-inputs witness was not written: {error!r}"
        print(message, file=_sys.stderr)
        return False
    return True


def _run(
    options: _RunnerOptions, environment: dict[bytes, bytes], command: list[str], seen: dict
) -> tuple[int, str | None, bytes]:
    # The exit status, why the runner itself decided it when it is not the test's own, and
    # the test's standard output when it was relayed.
    name = command[-1]
    try:
        environment = _fresh_dir_environment(environment, options.fresh_dir_env)
    except OSError as error:
        reason = f"not run: cannot make its fresh directory: {error}"
        print(f"error: {name}: {reason}", file=_sys.stderr)
        return _USAGE_EXIT, reason, b""
    if options.clear_env or options.fresh_dir_env:
        absent = [n for n in options.clear_env if _os.fsencode(n) not in environment]
        fresh = [f"{n}={_os.fsdecode(environment[_os.fsencode(n)])}" for n in options.fresh_dir_env]
        print(f"{name}: cleared {absent}; fresh {fresh}", file=_sys.stderr, flush=True)
    groups = _LiveCaseGroups()
    for number in _FORWARDED_SIGNALS:
        _signal.signal(number, lambda received, _frame: groups.stop_all(received))
    _sys.stdout.flush()
    _sys.stderr.flush()
    relayed = _subprocess.PIPE if options.spawn_inputs_witness else None
    seen.update(environment=environment, launching=True)
    process = groups.launch(
        lambda: _subprocess.Popen(
            command, env=environment, stdin=_subprocess.DEVNULL, stdout=relayed, process_group=0
        )
    )
    if isinstance(process, int):
        reason = f"not run: the runner received {_signal.Signals(process).name}"
        print(f"error: {name}: {reason}", file=_sys.stderr)
        return 128 + process, reason, b""
    seen.update(pid=process.pid)
    relay = _RawOneCaseRelay(process.stdout.fileno()) if relayed is not None else None
    try:
        exited = _wait_for_exit(process, options.per_test_timeout_seconds)
        # Whatever the test left running in its group would hold the output open; the
        # test itself, exited or not, is stopped with it.
        _os.killpg(process.pid, _signal.SIGKILL)
    finally:
        groups.finish(process)
        stdout, sink_error = relay.stop() if relay is not None else (b"", None)
        if relay is not None:
            process.stdout.close()
    returncode = process.wait()
    if sink_error is not None:
        # The test's output did not reach the runner's own, so the case cannot pass.
        reason = f"its output could not be relayed: {sink_error!r}"
        print(f"error: {name}: {reason}", file=_sys.stderr)
        return (returncode if returncode > 0 else _USAGE_EXIT), reason, stdout
    if not exited:
        reason = f"timed out after {options.per_test_timeout_seconds} seconds and was killed"
        print(f"error: {name}: {reason}", file=_sys.stderr)
        return _TIMEOUT_EXIT, reason, stdout
    stopped_by = groups.stop_signal()
    if stopped_by is not None:
        stopped = f"stopped: the runner received {_signal.Signals(stopped_by).name}"
        return 128 + stopped_by, stopped, stdout
    return (returncode if returncode >= 0 else 128 - returncode), None, stdout
