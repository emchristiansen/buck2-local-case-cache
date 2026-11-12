"""Parse `[--per-test-timeout-seconds N] -- BINARY [libtest arguments]` into runner options.

Among the libtest arguments, eight are the runner's own and never reach libtest:
`--declared-skip NAME`, a target's declared exact test name, which the runner leaves
out; `--declared-only-substring WORD` and `--declared-skip-substring WORD`, a target's declared
substrings, which narrow the caller's selection; and `--project-relative-env NAME`, a
variable holding a path relative to the project root, which the runner makes absolute
before any test sees it; `--project-relative-dir PATH`, a directory relative to the
project root, which the runner creates before any test runs; `--fresh-dir-env NAME`,
a variable each test process receives naming a new, empty directory of its own;
`--clear-env NAME`, a variable no process the runner starts receives; and
`--path-prefix-of-exe PATH`, an executable whose directory leads the PATH of every such process.
`--raw-one-case` asks for the one exactly selected test's own output and status instead
of the runner's report; see `run_raw_one_case`.
"""

import os as _os

from libtest_process_runner.check_disjoint_names import check_disjoint_names as _disjoint
from libtest_process_runner.constants import PATH_PREFIX_OF_EXE as _PATH_PREFIX
from libtest_process_runner.constants import RAW_ONE_CASE as _RAW_ONE_CASE
from libtest_process_runner.declared_word import declared_word as _declared_word
from libtest_process_runner.environment_name_option import environment_name_option as _name
from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode
from libtest_process_runner.listing_options import listing_options as _listing_options
from libtest_process_runner.option_value import option_value as _value
from libtest_process_runner.path_prefix_option import path_prefix_option as _prefix
from libtest_process_runner.positive_number import positive_number as _positive
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError

_TIMEOUT_OPTION = "--per-test-timeout-seconds"
_LIST = "--list"
_END_OF_OPTIONS = "--"
_SHOW_OUTPUT_FLAGS = frozenset({"--nocapture", "--no-capture", "--show-output"})
_DECLARED_SKIP = "--declared-skip"
_DECLARED_ONLY_SUBSTRING = "--declared-only-substring"
_DECLARED_SKIP_SUBSTRING = "--declared-skip-substring"
_PROJECT_RELATIVE_ENV = "--project-relative-env"
_PROJECT_RELATIVE_DIR = "--project-relative-dir"
_FRESH_DIR_ENV = "--fresh-dir-env"
_CLEAR_ENV = "--clear-env"
# Options whose next argument is their value, in the runner's grammar or in libtest's,
# which parses what follows `--list`. A route selection given as such a value is no
# route selection.
_TAKES_VALUE = frozenset(
    {
        _DECLARED_SKIP,
        _DECLARED_ONLY_SUBSTRING,
        _DECLARED_SKIP_SUBSTRING,
        _PROJECT_RELATIVE_ENV,
        _PROJECT_RELATIVE_DIR,
        _FRESH_DIR_ENV,
        _CLEAR_ENV,
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


def _split_runner_options(argv: list[str]) -> tuple[float | None, list[str]]:
    timeout = None
    index = 0
    while index < len(argv) and argv[index] != "--":
        if argv[index] != _TIMEOUT_OPTION or index + 1 >= len(argv) or timeout is not None:
            raise _RunnerUsageError(f"unexpected runner argument {argv[index]!r}")
        timeout = float(_positive(argv[index + 1], _TIMEOUT_OPTION, float))
        index += 2
    if index >= len(argv) or index + 1 >= len(argv):
        raise _RunnerUsageError("expected `-- BINARY [libtest arguments]`")
    return timeout, argv[index + 1 :]


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


def _check_route_selections(arguments: list[str]) -> None:
    # Each route is chosen once, and the two exclude each other whatever their order,
    # so the runner refuses before it starts any process.
    selections = _route_selections(arguments)
    for selection in (_LIST, _RAW_ONE_CASE):
        if selections.count(selection) > 1:
            raise _RunnerUsageError(f"{selection} is given more than once")
    if len(selections) == 2:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} cannot be combined with {_LIST}")


def _check_raw_one_case(
    exact: bool,
    filters: list[str],
    skips: list[str],
    declared: list[str],
    mode: _IgnoredMode,
    show_output: bool,
) -> None:
    # The test's own output must be libtest's default output for exactly the one test
    # named, so nothing may widen, narrow or reshape it.
    if not exact or len(filters) != 1:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} needs --exact and exactly one test name")
    if skips or declared:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} cannot be combined with a skip")
    if mode is not _IgnoredMode.SKIP or show_output:
        raise _RunnerUsageError(f"{_RAW_ONE_CASE} cannot change what libtest runs or prints")


def parse_runner_arguments(argv: list[str], environment: dict[bytes, bytes]) -> _RunnerOptions:
    timeout, rest = _split_runner_options(argv)
    binary, arguments = rest[0], rest[1:]
    _check_route_selections(arguments)
    filters, skips, declared_skips, project_relative_env, project_relative_dirs = [], [], [], [], []
    declared_only_substrings, declared_skip_substrings = [], []
    named = {_FRESH_DIR_ENV: [], _CLEAR_ENV: []}
    # Positions of the runner's own variable and directory arguments, which a
    # listing must not forward.
    runner_own = set()
    exact = show_output = raw_one_case = False
    ignored = only = False
    threads = prefix = None
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == _LIST:
            # The listing is the binary's own, which cannot leave a declared test out.
            # The binary itself parses what follows `--list`, so a later declared
            # selection reaches it and it rejects the unknown option. A project-relative
            # or cleared variable before `--list` applies to the listing too.
            declared = [
                flag
                for flag, given in (
                    (_DECLARED_SKIP, declared_skips),
                    (_DECLARED_ONLY_SUBSTRING, declared_only_substrings),
                    (_DECLARED_SKIP_SUBSTRING, declared_skip_substrings),
                )
                if given
            ]
            if declared:
                raise _RunnerUsageError(f"{declared[0]} cannot be combined with --list")
            return _listing_options(
                binary,
                timeout,
                tuple(project_relative_env),
                tuple(project_relative_dirs),
                tuple(named[_CLEAR_ENV]),
                prefix,
                tuple(
                    forwarded
                    for position, forwarded in enumerate(arguments)
                    if position not in runner_own
                ),
            )
        if argument == "--exact":
            exact, index = True, index + 1
        elif argument == _RAW_ONE_CASE:
            raw_one_case, index = True, index + 1
        elif argument == "--skip" or argument.startswith("--skip="):
            skip, index = _value(arguments, index, "--skip")
            skips.append(skip)
        elif argument == _DECLARED_SKIP or argument.startswith(_DECLARED_SKIP + "="):
            declared, index = _value(arguments, index, _DECLARED_SKIP)
            declared_skips.append(declared)
        elif argument == _DECLARED_ONLY_SUBSTRING or argument.startswith(
            _DECLARED_ONLY_SUBSTRING + "="
        ):
            index = _declared_word(
                arguments, index, _DECLARED_ONLY_SUBSTRING, declared_only_substrings
            )
        elif argument == _DECLARED_SKIP_SUBSTRING or argument.startswith(
            _DECLARED_SKIP_SUBSTRING + "="
        ):
            index = _declared_word(
                arguments, index, _DECLARED_SKIP_SUBSTRING, declared_skip_substrings
            )
        elif argument == _PROJECT_RELATIVE_ENV or argument.startswith(_PROJECT_RELATIVE_ENV + "="):
            start = index
            name, index = _value(arguments, index, _PROJECT_RELATIVE_ENV)
            if not name or "=" in name:
                raise _RunnerUsageError(
                    f"{_PROJECT_RELATIVE_ENV} needs a variable name, got {name!r}"
                )
            if name in project_relative_env:
                raise _RunnerUsageError(f"{_PROJECT_RELATIVE_ENV} names {name!r} twice")
            project_relative_env.append(name)
            runner_own.update(range(start, index))
        elif argument == _PROJECT_RELATIVE_DIR or argument.startswith(_PROJECT_RELATIVE_DIR + "="):
            start = index
            path, index = _value(arguments, index, _PROJECT_RELATIVE_DIR)
            if not path or path in project_relative_dirs:
                raise _RunnerUsageError(f"{_PROJECT_RELATIVE_DIR} needs a new path, got {path!r}")
            project_relative_dirs.append(path)
            runner_own.update(range(start, index))
        elif argument.partition("=")[0] in named:
            start, flag = index, argument.partition("=")[0]
            index = _name(arguments, index, flag, named[flag])
            runner_own.update(range(start, index))
        elif argument.partition("=")[0] == _PATH_PREFIX:
            start = index
            prefix, index = _prefix(arguments, index, prefix)
            runner_own.update(range(start, index))
        elif argument == "--test-threads" or argument.startswith("--test-threads="):
            text, index = _value(arguments, index, "--test-threads")
            threads = int(_positive(text, "--test-threads", int))
        elif argument in _SHOW_OUTPUT_FLAGS:
            show_output, index = True, index + 1
        elif argument == "--ignored":
            only, index = True, index + 1
        elif argument == "--include-ignored":
            ignored, index = True, index + 1
        elif argument.startswith("-"):
            raise _RunnerUsageError(f"unsupported libtest argument {argument!r}")
        else:
            filters.append(argument)
            index += 1
    _disjoint(((_PROJECT_RELATIVE_ENV, project_relative_env), *named.items()))
    if ignored and only:
        raise _RunnerUsageError("--ignored and --include-ignored cannot both be given")
    if threads is None:
        from_environment = environment.get(b"RUST_TEST_THREADS")
        threads = (
            int(_positive(from_environment.decode(), "RUST_TEST_THREADS", int))
            if from_environment is not None
            else _os.process_cpu_count()
        )
        if threads is None:
            raise _RunnerUsageError("cannot determine the CPU count; pass --test-threads")
    mode = _IgnoredMode.ONLY if only else _IgnoredMode.INCLUDE if ignored else _IgnoredMode.SKIP
    if raw_one_case:
        _check_raw_one_case(
            exact,
            filters,
            skips,
            declared_skips + declared_only_substrings + declared_skip_substrings,
            mode,
            show_output,
        )
    return _RunnerOptions(
        binary=binary,
        filters=tuple(filters),
        skips=tuple(skips),
        exact=exact,
        declared_skips=tuple(declared_skips),
        declared_only_substrings=tuple(declared_only_substrings),
        declared_skip_substrings=tuple(declared_skip_substrings),
        project_relative_env=tuple(project_relative_env),
        project_relative_dirs=tuple(project_relative_dirs),
        fresh_dir_env=tuple(named[_FRESH_DIR_ENV]),
        clear_env=tuple(named[_CLEAR_ENV]),
        path_prefix_of_exe=prefix,
        ignored_mode=mode,
        show_output=show_output,
        test_threads=threads,
        per_test_timeout_seconds=timeout,
        raw_one_case=raw_one_case,
        list_only=False,
        list_arguments=(),
    )
