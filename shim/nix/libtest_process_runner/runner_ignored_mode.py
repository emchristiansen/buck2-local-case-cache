"""Which ignored tests a run selects, from libtest's two flags and the declared ignored names."""

from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode
from libtest_process_runner.runner_usage_error import (
    RunnerUsageError as _RunnerUsageError,
)


def runner_ignored_mode(
    only: bool, ignored: bool, declared_flag: str, declared: list[str]
) -> _IgnoredMode:
    """`--ignored` (`only`) selects only ignored tests and `--include-ignored` (`ignored`)
    every test; the two together, or either with a declared ignored name (`declared_flag`),
    which already runs exactly its tests, are refused."""
    if ignored and only:
        raise _RunnerUsageError("--ignored and --include-ignored cannot both be given")
    if declared and (ignored or only):
        # Either flag already runs every ignored test it selects.
        flag = "--ignored" if only else "--include-ignored"
        raise _RunnerUsageError(f"{declared_flag} cannot be combined with {flag}")
    return (
        _IgnoredMode.ONLY
        if only
        else _IgnoredMode.INCLUDE
        if ignored
        else _IgnoredMode.SKIP
    )
