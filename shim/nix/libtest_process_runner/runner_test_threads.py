"""The number of tests a run executes at once, when `--test-threads` does not give it."""

import os as _os

from libtest_process_runner.positive_number import positive_number as _positive
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def runner_test_threads(environment: dict[bytes, bytes]) -> int:
    """`RUST_TEST_THREADS` when the environment sets it, as libtest reads it, else the
    process's CPU count; a run whose count neither gives is refused."""
    from_environment = environment.get(b"RUST_TEST_THREADS")
    threads = (
        int(_positive(from_environment.decode(), "RUST_TEST_THREADS", int))
        if from_environment is not None
        else _os.process_cpu_count()
    )
    if threads is None:
        raise _RunnerUsageError("cannot determine the CPU count; pass --test-threads")
    return threads
