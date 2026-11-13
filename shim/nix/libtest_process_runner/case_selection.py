"""Which listed tests run in their own process, and which are reported ignored unrun."""

from dataclasses import dataclass as _dataclass


@_dataclass(frozen=True)
class CaseSelection:
    to_run: tuple[str, ...]
    reported_ignored: tuple[str, ...]
    # Tests in the binary that the filters, skips or ignored mode left out.
    filtered_out: int
