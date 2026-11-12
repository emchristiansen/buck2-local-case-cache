"""Add one environment-variable option's name to the names that option collects."""

from libtest_process_runner.declared_word import declared_word as _declared_word
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def environment_name_option(arguments: list[str], index: int, flag: str, names: list[str]) -> int:
    # Returns the index of the argument after the name. A name holding "=" is no
    # variable, and TMPDIR is the run's own temporary directory.
    index = _declared_word(arguments, index, flag, names)
    if "=" in names[-1] or names[-1] == "TMPDIR":
        raise _RunnerUsageError(f"{flag} cannot name {names[-1]!r}")
    return index
