"""Refuse a variable that two of the runner's environment options both name."""

from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def check_disjoint_names(named: tuple[tuple[str, list[str]], ...]) -> None:
    # Each pair is an option and the names it was given. A variable may be made
    # absolute, made a fresh directory or cleared, but only one of them.
    for position, (flag, names) in enumerate(named):
        for other, other_names in named[position + 1 :]:
            overlap = sorted(set(names) & set(other_names))
            if overlap:
                raise _RunnerUsageError(f"{flag} and {other} name {overlap!r}")
