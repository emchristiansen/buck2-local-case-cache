"""The runner's two route selections, `--list` and `--raw-one-case`, and what a raw case may hold.

Each route is chosen once and the two exclude each other, whatever their order, so the
runner refuses before it starts any process. A raw case's output must be libtest's
default output for exactly the one test named, so nothing may widen, narrow or reshape it.
"""

from libtest_process_runner.constants import PATH_PREFIX_OF_EXE as _PATH_PREFIX
from libtest_process_runner.constants import RAW_ONE_CASE as _RAW_ONE_CASE
from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError

_LIST = "--list"
_END_OF_OPTIONS = "--"
# Options whose next argument is their value, in the runner's grammar or in libtest's,
# which parses what follows `--list`. A route selection given as such a value is no
# route selection.
_TAKES_VALUE = frozenset(
    {
        "--declared-skip",
        "--declared-ignored",
        "--declared-only-substring",
        "--declared-skip-substring",
        "--project-relative-env",
        "--project-relative-dir",
        "--fresh-dir-env",
        "--clear-env",
        _PATH_PREFIX,
        "--skip",
        "--test-threads",
        "--format",
        "--logfile",
        "--color",
        "--shuffle-seed",
        "-Z",
    }
)


def _route_selections(arguments: list[str]) -> list[str]:
    """`--list` and `--raw-one-case` as given in `arguments` as options.

    Neither an option's value nor anything after a `--` that ends the options is one:
    libtest reads every argument after that `--` as a test name, so the runner passes
    it on as such.
    """
    selections = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == _END_OF_OPTIONS:
            break
        if argument in (_LIST, _RAW_ONE_CASE):
            selections.append(argument)
        index += 2 if argument in _TAKES_VALUE else 1
    return selections


def check_route_selections(arguments: list[str]) -> None:
    selections = _route_selections(arguments)
    for selection in (_LIST, _RAW_ONE_CASE):
        if selections.count(selection) > 1:
            raise _RunnerUsageError(f"{selection} is given more than once")
    if len(selections) == 2:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} cannot be combined with {_LIST}")


def checked_raw_one_case(options: _RunnerOptions) -> _RunnerOptions:
    """`options`, when they do not ask for a raw case or ask for one the runner can run."""
    if not options.raw_one_case:
        return options
    if not options.exact or len(options.filters) != 1:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} needs --exact and exactly one test name")
    declared = (
        options.declared_skips + options.declared_only_substrings + options.declared_skip_substrings
    )
    if options.skips or declared:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} cannot be combined with a skip")
    # A declared ignored name would run its test under `--include-ignored`.
    reshaped = options.ignored_mode is not _IgnoredMode.SKIP or options.show_output
    if reshaped or options.declared_ignored:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} cannot change what libtest runs or prints")
    return options
