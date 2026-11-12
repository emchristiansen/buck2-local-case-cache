"""Run one selected test in its own process, as nextest runs it."""

import os as _os
import signal as _signal
import subprocess as _subprocess
import sys as _sys

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

_IGNORED_FLAGS = {
    _IgnoredMode.SKIP: [],
    _IgnoredMode.ONLY: ["--ignored"],
    _IgnoredMode.INCLUDE: ["--include-ignored"],
}


def run_case(
    options: _RunnerOptions, name: str, environment: dict[bytes, bytes], groups: _LiveCaseGroups
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
        *_IGNORED_FLAGS[options.ignored_mode],
    ]
    try:
        environment = _fresh_dir_environment(environment, options.fresh_dir_env)
    except OSError as error:
        reason = f"not run: cannot make its fresh directory: {error}"
        return _CaseResult(name, _CaseOutcome.FAILED, reason, b"", False)
    if options.clear_env or options.fresh_dir_env:
        # Read from the environment the test receives: the cleared names it lacks, by
        # name only, and each fresh directory.
        absent = [n for n in options.clear_env if _os.fsencode(n) not in environment]
        fresh = [f"{n}={_os.fsdecode(environment[_os.fsencode(n)])}" for n in options.fresh_dir_env]
        print(f"{name}: cleared {absent}; fresh {fresh}", file=_sys.stderr, flush=True)
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
    if isinstance(process, int):
        reason = f"not run: the runner received {_signal.Signals(process).name}"
        return _CaseResult(name, _CaseOutcome.FAILED, reason, b"", False)
    collected = _collect_case_output(process, options.per_test_timeout_seconds, groups)
    return _classify_case(name, collected, options.per_test_timeout_seconds)
