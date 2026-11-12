"""Read the one executable whose directory the runner puts first on each process's PATH."""

from libtest_process_runner.constants import PATH_PREFIX_OF_EXE as _PATH_PREFIX_OF_EXE
from libtest_process_runner.option_value import option_value as _option_value
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def path_prefix_option(arguments: list[str], index: int, given: str | None) -> tuple[str, int]:
    # Returns the path and the index of the argument after it; the option is given once.
    if given is not None:
        raise _RunnerUsageError(f"{_PATH_PREFIX_OF_EXE} can be given only once")
    return _option_value(arguments, index, _PATH_PREFIX_OF_EXE)
