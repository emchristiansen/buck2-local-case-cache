"""Run one selected test in its own process, as nextest runs it."""

import os as _os
import signal as _signal
import subprocess as _subprocess

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome
from libtest_process_runner.case_result import CaseResult as _CaseResult
from libtest_process_runner.classify_case import classify_case as _classify_case
from libtest_process_runner.collect_case_output import collect_case_output as _collect_case_output
from libtest_process_runner.fresh_dir_environment import (
    fresh_dir_environment as _fresh_dir_environment,
)
from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode
from libtest_process_runner.live_case_groups import LiveCaseGroups as _LiveCaseGroups
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.spawn_inputs_witness import (
    spawn_inputs_witness as _spawn_inputs_witness,
)
from libtest_process_runner.stderr_line import write_stderr_line as _write_stderr_line

_IGNORED_FLAGS = {
    _IgnoredMode.SKIP: [],
    _IgnoredMode.ONLY: ["--ignored"],
    _IgnoredMode.INCLUDE: ["--include-ignored"],
}


def run_case(
    options: _RunnerOptions, name: str, environment: dict[bytes, bytes], groups: _LiveCaseGroups
) -> _CaseResult:
    if not options.spawn_inputs_witness:
        return _run_case(options, name, environment, groups, None)
    seen = {}
    try:
        result = _run_case(options, name, environment, groups, seen)
    except OSError as error:
        if "launch" not in seen:
            raise
        # The launch itself failed: no child exists. Record that, then fail as before.
        refused = _CaseResult(name, _CaseOutcome.FAILED, f"not started: {error!r}", b"", False)
        try:
            _spawn_inputs_witness(refused, seen["command"], seen["cwd"], seen["environment"], None)
        except Exception as failure:
            error.add_note(f"its spawn-inputs witness was not written: {failure!r}")
        raise
    try:
        _spawn_inputs_witness(
            result, seen["command"], seen["cwd"], seen["environment"], seen.get("pid")
        )
    except Exception as error:
        # Missing evidence never passes for a recorded case.
        reason = f"its spawn-inputs witness was not written: {error!r}"
        return _CaseResult(
            name, _CaseOutcome.FAILED, reason, result.output, result.left_output_open
        )
    return result


def _run_case(
    options: _RunnerOptions,
    name: str,
    environment: dict[bytes, bytes],
    groups: _LiveCaseGroups,
    seen: dict | None,
) -> _CaseResult:
    # The same arguments nextest passes. The test inherits the runner's working
    # directory and receives the runner's starting environment, with only its
    # project-relative variables made absolute, its cleared variables removed, any
    # PATH prefix applied, TMPDIR set to the run's own temporary directory and each fresh-directory
    # variable naming its own.
    command = [
        options.binary,
        name,
        "--exact",
        "--nocapture",
        # libtest runs an ignored test only under `--include-ignored`.
        *(
            ["--include-ignored"]
            if name in options.declared_ignored
            else _IGNORED_FLAGS[options.ignored_mode]
        ),
    ]
    if seen is not None:
        # The runner never changes directory, so the child inherits this one exactly.
        seen.update(command=command, cwd=_os.getcwdb(), environment=None)
    try:
        environment = _fresh_dir_environment(environment, options.fresh_dir_env)
    except OSError as error:
        reason = f"not run: cannot make its fresh directory: {error}"
        return _CaseResult(name, _CaseOutcome.FAILED, reason, b"", False)
    if options.clear_env or options.fresh_dir_env:
        # Read from the environment the test receives: the cleared names it lacks, by
        # name only, and each fresh directory. One whole line, so another case's witness
        # never lands inside it.
        absent = [n for n in options.clear_env if _os.fsencode(n) not in environment]
        fresh = [f"{n}={_os.fsdecode(environment[_os.fsencode(n)])}" for n in options.fresh_dir_env]
        _write_stderr_line(f"{name}: cleared {absent}; fresh {fresh}")
    if seen is not None:
        seen.update(environment=environment, launch=True)
    process = groups.launch(
        lambda: _subprocess.Popen(
            command,
            env=environment,
            stdin=_subprocess.DEVNULL,
            stdout=_subprocess.PIPE,
            stderr=_subprocess.STDOUT,
            process_group=0,
        )
    )
    if seen is not None:
        del seen["launch"]
    if isinstance(process, int):
        reason = f"not run: the runner received {_signal.Signals(process).name}"
        return _CaseResult(name, _CaseOutcome.FAILED, reason, b"", False)
    if seen is not None:
        seen["pid"] = process.pid
    collected = _collect_case_output(process, options.per_test_timeout_seconds, groups)
    return _classify_case(name, collected, options.per_test_timeout_seconds)
