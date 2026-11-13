"""Print libtest's closing sections and summary line, and return the runner's exit status."""

import sys as _sys

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome
from libtest_process_runner.case_result import CaseResult as _CaseResult
from libtest_process_runner.constants import LIBTEST_FAILURE_EXIT as _FAILURE_EXIT


def _section(title: str, results: list[_CaseResult]) -> bytes:
    text = f"\n{title}:\n".encode()
    for result in results:
        text += f"\n---- {result.name} stdout ----\n".encode() + result.output
        if result.output and not result.output.endswith(b"\n"):
            text += b"\n"
        if result.reason is not None:
            text += f"note: {result.name}: {result.reason}\n".encode()
    return text


def report_summary(
    results: list[_CaseResult], filtered_out: int, show_output: bool, elapsed_seconds: float
) -> int:
    def having(outcome: _CaseOutcome) -> list[_CaseResult]:
        return sorted((r for r in results if r.outcome is outcome), key=lambda r: r.name)

    passed = having(_CaseOutcome.PASSED)
    failed = having(_CaseOutcome.FAILED)
    ignored = having(_CaseOutcome.IGNORED)
    text = b""
    if show_output and passed:
        text += _section("successes", passed)
    if failed:
        text += _section("failures", failed)
        text += b"\nfailures:\n" + b"".join(f"    {r.name}\n".encode() for r in failed)
    verdict = "FAILED" if failed else "ok"
    text += (
        f"\ntest result: {verdict}. {len(passed)} passed; {len(failed)} failed; "
        f"{len(ignored)} ignored; 0 measured; {filtered_out} filtered out; "
        f"finished in {elapsed_seconds:.2f}s\n\n"
    ).encode()
    _sys.stdout.buffer.write(text)
    _sys.stdout.flush()
    return _FAILURE_EXIT if failed else 0
