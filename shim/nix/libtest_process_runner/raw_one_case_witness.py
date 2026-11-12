"""The spawn-inputs witness of a raw one case, decided from libtest's own result line for it.

A witnessed raw case relays the test's standard output unchanged while keeping a copy, so
after the test ends the runner reads the one result line libtest printed for exactly the
case named and records the case as the External route records a case it ran: a case libtest
ran and reported `ok`, with exit status 0, as PASSED; one it reported `FAILED` as FAILED;
one it ignored with no record at all, since its body never ran. Any other ending (no
complete opening, a result line for another test, a timeout, a signal, an `ok` whose
status is not 0) is a started case recorded as FAILED with the reason, so missing evidence
never reads as a pass. The strict parser that reads the same output stays the only judge
of the case; this record only binds the case's spawn inputs to what libtest reported.
"""

from libtest_process_runner.case_outcome import CaseOutcome as _CaseOutcome
from libtest_process_runner.case_result import CaseResult as _CaseResult

_SHOULD_PANIC = " - should panic"


def _reported(name: str, stdout: bytes) -> str | None:
    """libtest's outcome word for exactly `name`, read where libtest prints it, or None."""
    lines = stdout.decode("utf-8", "replace").split("\n")
    if len(lines) < 3 or lines[0] != "" or lines[1] != "running 1 test":
        return None
    reported, separator, outcome = lines[2].removeprefix("test ").partition(" ... ")
    if not lines[2].startswith("test ") or not separator:
        return None
    if reported.removesuffix(_SHOULD_PANIC) != name:
        return None
    if outcome in ("ok", "FAILED"):
        return outcome
    return "ignored" if outcome == "ignored" or outcome.startswith("ignored, ") else None


def raw_case_record(
    name: str, stdout: bytes, status: int, reason: str | None
) -> _CaseResult | None:
    """The result to witness for a raw case that started, or None for an ignored case."""
    outcome = _reported(name, stdout) if reason is None else None
    if outcome == "ignored" and status == 0:
        return None
    if outcome == "ok" and status == 0:
        return _CaseResult(name, _CaseOutcome.PASSED, None, b"", False)
    if outcome == "FAILED":
        return _CaseResult(name, _CaseOutcome.FAILED, None, b"", False)
    why = reason or (
        f"exit status {status} with libtest's result line `{outcome}`"
        if outcome is not None
        else f"exit status {status} without a libtest result line for {name}"
    )
    return _CaseResult(name, _CaseOutcome.FAILED, why, b"", False)
