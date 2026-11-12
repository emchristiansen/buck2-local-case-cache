"""Run a libtest binary's selected tests, each in its own process, and report like libtest."""

import concurrent.futures as _futures
import os as _os
import shutil as _shutil
import signal as _signal
import subprocess as _subprocess
import sys as _sys
import tempfile as _tempfile
import time as _time

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome
from libtest_process_runner.case_result import CaseResult as _CaseResult
from libtest_process_runner.constants import FALLBACK_TEMPORARY_ROOT as _FALLBACK_TEMPORARY_ROOT
from libtest_process_runner.constants import SHARED_TEMPORARY_ROOTS as _SHARED_TEMPORARY_ROOTS
from libtest_process_runner.constants import USAGE_EXIT as _USAGE_EXIT
from libtest_process_runner.live_case_groups import LiveCaseGroups as _LiveCaseGroups
from libtest_process_runner.parse_runner_arguments import parse_runner_arguments as _parse
from libtest_process_runner.report_case_line import report_case_line as _report_case_line
from libtest_process_runner.report_summary import report_summary as _report_summary
from libtest_process_runner.run_raw_one_case import run_raw_one_case as _run_raw_one_case
from libtest_process_runner.run_case import run_case as _run_case
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError
from libtest_process_runner.select_cases import select_cases as _select_cases
from libtest_process_runner.with_path_prefix import with_path_prefix as _with_path_prefix
from process_environment.initial_environment import initial_environment as _initial_environment

_FORWARDED_SIGNALS = (_signal.SIGTERM, _signal.SIGINT, _signal.SIGHUP)


def _with_project_relative_paths(
    environment: dict[bytes, bytes], names: tuple[str, ...]
) -> dict[bytes, bytes]:
    # `buck2 test` runs the runner in the project root, so each declared value is
    # joined to the working directory and normalized lexically, never resolving a
    # symbolic link; "." is the project root itself. A value that climbs out of
    # the project root is refused like an absolute one. Every other variable
    # passes through unchanged.
    rewritten = dict(environment)
    root = _os.getcwdb()
    for name in names:
        key = _os.fsencode(name)
        value = environment.get(key)
        if value is None:
            raise _RunnerUsageError(f"project-relative variable {name} is not set")
        if not value:
            raise _RunnerUsageError(f"project-relative variable {name} is empty")
        if _os.path.isabs(value):
            raise _RunnerUsageError(f"project-relative variable {name} is absolute: {value!r}")
        absolute = _os.path.normpath(_os.path.join(root, value))
        if _os.path.commonpath([root, absolute]) != root:
            raise _RunnerUsageError(
                f"project-relative variable {name} leaves the project root: {value!r}"
            )
        rewritten[key] = absolute
    return rewritten


def _create_project_relative_dirs(paths: tuple[str, ...]) -> None:
    # A compile-time path such as CARGO_TARGET_TMPDIR names a directory the test
    # expects to exist, as Cargo makes it; it is checked like a project-relative value.
    root = _os.getcwd()
    for path in paths:
        absolute = _os.path.normpath(_os.path.join(root, path))
        if _os.path.isabs(path) or _os.path.commonpath([root, absolute]) != root:
            raise _RunnerUsageError(f"project-relative directory leaves the project root: {path!r}")
        _os.makedirs(absolute, exist_ok=True)


def _run_temporary_directory(environment: dict[bytes, bytes]) -> bytes:
    # Buck2 hands the runner its daemon's TMPDIR, which may be unset or shared, so
    # every run gets a fresh private directory that is removed when the run ends.
    own = environment.get(b"TMPDIR")
    parent = None
    if own and _os.path.isabs(own):
        resolved = _os.path.realpath(own)
        if not any(
            _os.path.commonpath([resolved, shared]) == shared for shared in _SHARED_TEMPORARY_ROOTS
        ):
            parent = own
    try:
        if parent is None:
            home = environment.get(b"HOME")
            if not home or not _os.path.isabs(home):
                raise _RunnerUsageError(
                    "TMPDIR is unset or shared and HOME is not an absolute path, "
                    "so no directory is left for the tests' temporary files"
                )
            # Only the fallback is made here; the runner's own TMPDIR must exist.
            parent = _os.path.join(home, _FALLBACK_TEMPORARY_ROOT)
            _os.makedirs(parent, mode=0o700, exist_ok=True)
        return _tempfile.mkdtemp(prefix=b"run-", dir=parent)
    except OSError as error:
        raise _RunnerUsageError(
            f"cannot make the run's temporary directory in {_os.fsdecode(parent)}: {error}"
        ) from error


def _report_leftover(_function: object, path: bytes, error: BaseException) -> None:
    # A test may leave an entry the runner cannot remove. Its run keeps the status it
    # earned; the leftover is named rather than replacing that status.
    print(f"left behind: {_os.fsdecode(path)} ({error})", file=_sys.stderr)


def _run(options: _RunnerOptions, environment: dict[bytes, bytes]) -> int:
    if options.list_only:
        listing = _subprocess.run(
            [options.binary, *options.list_arguments], env=environment, check=False
        )
        return listing.returncode if listing.returncode >= 0 else 128 - listing.returncode
    if options.raw_one_case:
        return _run_raw_one_case(options, environment)
    started = _time.monotonic()
    selection = _select_cases(options, environment)
    groups = _LiveCaseGroups()
    for number in _FORWARDED_SIGNALS:
        _signal.signal(number, lambda received, _frame: groups.stop_all(received))
    count = len(selection.to_run) + len(selection.reported_ignored)
    print(f"\nrunning {count} test{'' if count == 1 else 's'}", flush=True)
    results = [
        _CaseResult(name, _CaseOutcome.IGNORED, None, b"", False)
        for name in selection.reported_ignored
    ]
    for result in results:
        _report_case_line(result)
    with _futures.ThreadPoolExecutor(max_workers=options.test_threads) as pool:
        pending = [
            pool.submit(_run_case, options, name, environment, groups) for name in selection.to_run
        ]
        for finished in _futures.as_completed(pending):
            result = finished.result()
            _report_case_line(result)
            results.append(result)
    status = _report_summary(
        results, selection.filtered_out, options.show_output, _time.monotonic() - started
    )
    stop_signal = groups.stop_signal()
    return status if stop_signal is None else 128 + stop_signal


def run_tests_in_processes(argv: list[str]) -> int:
    environment = _initial_environment()
    try:
        options = _parse(argv, environment)
        environment = _with_project_relative_paths(environment, options.project_relative_env)
        # A cleared variable reaches no process the runner starts, listings included.
        cleared = {_os.fsencode(name) for name in options.clear_env}
        environment = {name: value for name, value in environment.items() if name not in cleared}
        # So does the PATH prefix, checked before anything is created.
        environment = _with_path_prefix(environment, options)
        _create_project_relative_dirs(options.project_relative_dirs)
        temporary = _run_temporary_directory(environment)
    except _RunnerUsageError as error:
        print(f"error: {error}", file=_sys.stderr)
        return _USAGE_EXIT
    try:
        return _run(options, {**environment, b"TMPDIR": temporary})
    finally:
        _shutil.rmtree(temporary, onexc=_report_leftover)
