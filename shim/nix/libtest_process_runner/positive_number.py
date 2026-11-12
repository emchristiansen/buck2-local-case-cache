"""Read a runner argument that must be a positive number."""

from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def positive_number(text: str, what: str, kind: type) -> float | int:
    try:
        value = kind(text)
    except ValueError as error:
        raise _RunnerUsageError(f"{what} must be a positive number, got {text!r}") from error
    if value <= 0:
        raise _RunnerUsageError(f"{what} must be a positive number, got {text!r}")
    return value
