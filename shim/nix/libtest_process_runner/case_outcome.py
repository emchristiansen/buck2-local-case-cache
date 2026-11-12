"""The result libtest would report for one test."""

from enum import Enum as _Enum


class CaseOutcome(_Enum):
    PASSED = "ok"
    FAILED = "FAILED"
    IGNORED = "ignored"
