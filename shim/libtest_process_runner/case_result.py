"""One test's outcome, the reason for a failure the libtest summary cannot show, and its output."""

from dataclasses import dataclass as _dataclass

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome


@_dataclass(frozen=True)
class CaseResult:
    name: str
    outcome: _CaseOutcome
    # Why the runner failed it, when libtest's own summary does not say: a signal,
    # a timeout, a missing summary, or a summary that does not account for one test.
    reason: str | None
    # Standard output and standard error together, in the order they were written.
    output: bytes
    # The process exited but other processes kept its output open past the leak timeout.
    left_output_open: bool
