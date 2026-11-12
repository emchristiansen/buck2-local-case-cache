"""What the runner observed of one test process."""

from dataclasses import dataclass as _dataclass


@_dataclass(frozen=True)
class CollectedCase:
    # `Popen.returncode`: negative for a process ended by a signal.
    returncode: int
    output: bytes
    timed_out: bool
    left_output_open: bool
    # The signal the runner itself received and forwarded, if it stopped this test.
    stopped_by_signal: int | None
