"""Turn one observed test process into the outcome libtest would have reported for it."""

import re as _re
import signal as _signal

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome
from libtest_process_runner.case_result import CaseResult as _CaseResult
from libtest_process_runner.collected_case import CollectedCase as _CollectedCase
from libtest_process_runner.constants import LIBTEST_FAILURE_EXIT as _FAILURE_EXIT
from libtest_process_runner.constants import LIBTEST_SUMMARY_PATTERN as _SUMMARY_PATTERN

_SUMMARY = _re.compile(_SUMMARY_PATTERN, _re.MULTILINE)


def _signal_name(number: int) -> str:
    try:
        return _signal.Signals(number).name
    except ValueError:
        return f"signal {number}"


def _judge(
    collected: _CollectedCase, timeout_seconds: float | None
) -> tuple[_CaseOutcome, str | None]:
    failed = _CaseOutcome.FAILED
    if collected.timed_out:
        return failed, f"timed out after {timeout_seconds} seconds and was killed"
    if collected.stopped_by_signal is not None:
        return failed, f"stopped: the runner received {_signal_name(collected.stopped_by_signal)}"
    if collected.returncode < 0:
        return failed, f"killed by {_signal_name(-collected.returncode)}"
    summaries = list(_SUMMARY.finditer(collected.output.decode(errors="replace")))
    if not summaries:
        return failed, f"exited with code {collected.returncode} without a libtest summary"
    counts = {key: int(value) for key, value in summaries[-1].groupdict().items()}
    if sum(counts.values()) != 1:
        return failed, f"libtest summary accounts for {sum(counts.values())} tests, not one"
    if counts["passed"] == 1 and collected.returncode == 0:
        return _CaseOutcome.PASSED, None
    if counts["failed"] == 1 and collected.returncode == _FAILURE_EXIT:
        # libtest's own failure: its output already says why.
        return failed, None
    if counts["ignored"] == 1:
        return failed, "libtest reported it ignored although the runner selected it to run"
    return failed, f"libtest summary {counts} disagrees with exit code {collected.returncode}"


def classify_case(
    name: str, collected: _CollectedCase, timeout_seconds: float | None
) -> _CaseResult:
    outcome, reason = _judge(collected, timeout_seconds)
    return _CaseResult(name, outcome, reason, collected.output, collected.left_output_open)
