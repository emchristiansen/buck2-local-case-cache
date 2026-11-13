"""Print one test's libtest result line as soon as the test finishes."""

import sys as _sys

from libtest_process_runner.case_result import CaseResult as _CaseResult


def report_case_line(result: _CaseResult) -> None:
    line = f"test {result.name} ... {result.outcome.value}\n"
    if result.left_output_open:
        line += f"note: {result.name}: left processes holding its output open after it exited\n"
    _sys.stdout.write(line)
    _sys.stdout.flush()
