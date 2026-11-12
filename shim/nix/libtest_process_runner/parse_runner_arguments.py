"""Parse `[--per-test-timeout-seconds N] -- BINARY [libtest arguments]` into runner options.

Among the libtest arguments, eleven are the runner's own and never reach libtest:
`--declared-skip NAME`, a target's declared exact test name, which the runner leaves
out; `--declared-ignored NAME`, a target's declared exact ignored test name, which the
runner runs; `--declared-only-substring WORD` and `--declared-skip-substring WORD`, a
target's declared substrings, which narrow the caller's selection; and
`--project-relative-env NAME`, a variable holding a path relative to the project root,
which the runner makes absolute before any test sees it; `--project-relative-dir PATH`,
a directory relative to the project root, which the runner creates before any test
runs; `--fresh-dir-env NAME`, a variable each test process receives naming a new, empty
directory of its own; `--clear-env NAME`, a variable no process the runner starts
receives; `--path-prefix-of-exe PATH`, an executable whose directory leads the PATH of
every such process; `--spawn-inputs-witness`, which has each case's final spawn
inputs written with its outcome; and `--raw-one-case`, which hands on the one exactly
selected test's own output and status in place of a report (run_raw_one_case).
"""

from libtest_process_runner.check_disjoint_names import check_disjoint_names as _disjoint
from libtest_process_runner.constants import PATH_PREFIX_OF_EXE as _PATH_PREFIX
from libtest_process_runner.declared_word import declared_word as _declared_word
from libtest_process_runner.environment_name_option import environment_name_option as _name
from libtest_process_runner.listing_options import listing_options as _listing_options
from libtest_process_runner.option_value import option_value as _value
from libtest_process_runner.path_prefix_option import path_prefix_option as _prefix
from libtest_process_runner.positive_number import positive_number as _positive
from libtest_process_runner.project_relative_option import relative_dir_option as _relative_dir
from libtest_process_runner.project_relative_option import relative_env_option as _relative_env
from libtest_process_runner.raw_one_case_route import check_route_selections as _check_routes
from libtest_process_runner.raw_one_case_route import checked_raw_one_case as _checked_raw
from libtest_process_runner.runner_ignored_mode import runner_ignored_mode as _runner_ignored_mode
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.runner_test_threads import runner_test_threads as _runner_test_threads
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError
from libtest_process_runner.split_runner_options import (
    split_runner_options as _split_runner_options,
)

_SHOW_OUTPUT_FLAGS = frozenset({"--nocapture", "--no-capture", "--show-output"})
_DECLARED_SKIP = "--declared-skip"
_DECLARED_IGNORED = "--declared-ignored"
_DECLARED_ONLY_SUBSTRING = "--declared-only-substring"
_DECLARED_SKIP_SUBSTRING = "--declared-skip-substring"
_PROJECT_RELATIVE_ENV = "--project-relative-env"
_PROJECT_RELATIVE_DIR = "--project-relative-dir"
_FRESH_DIR_ENV = "--fresh-dir-env"
_CLEAR_ENV = "--clear-env"
_SPAWN_INPUTS_WITNESS = "--spawn-inputs-witness"
_RAW_ONE_CASE = "--raw-one-case"


def parse_runner_arguments(argv: list[str], environment: dict[bytes, bytes]) -> _RunnerOptions:
    timeout, rest = _split_runner_options(argv)
    binary, arguments = rest[0], rest[1:]
    _check_routes(arguments)
    filters, skips, declared_skips, project_relative_env, project_relative_dirs = [], [], [], [], []
    declared_only_substrings, declared_skip_substrings, declared_ignored = [], [], []
    named = {_FRESH_DIR_ENV: [], _CLEAR_ENV: []}
    # Positions of the runner's own variable and directory arguments, which a
    # listing must not forward.
    runner_own = set()
    exact = show_output = witness = raw_one_case = False
    ignored = only = False
    threads = prefix = None
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--list":
            # The listing is the binary's own, which cannot leave a declared test out.
            # The binary itself parses what follows `--list`, so a later declared
            # selection reaches it and it rejects the unknown option. A project-relative
            # or cleared variable before `--list` applies to the listing too.
            declared = [
                flag
                for flag, given in (
                    (_DECLARED_SKIP, declared_skips),
                    (_DECLARED_IGNORED, declared_ignored),
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
        elif argument.partition("=")[0] == _DECLARED_IGNORED:
            index = _declared_word(arguments, index, _DECLARED_IGNORED, declared_ignored)
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
            index = _relative_env(arguments, index, project_relative_env)
            runner_own.update(range(start, index))
        elif argument == _PROJECT_RELATIVE_DIR or argument.startswith(_PROJECT_RELATIVE_DIR + "="):
            start = index
            index = _relative_dir(arguments, index, project_relative_dirs)
            runner_own.update(range(start, index))
        elif argument.partition("=")[0] in named:
            start, flag = index, argument.partition("=")[0]
            index = _name(arguments, index, flag, named[flag])
            runner_own.update(range(start, index))
        elif argument.partition("=")[0] == _PATH_PREFIX:
            start = index
            prefix, index = _prefix(arguments, index, prefix)
            runner_own.update(range(start, index))
        elif argument == _SPAWN_INPUTS_WITNESS:
            if witness:
                raise _RunnerUsageError(f"{_SPAWN_INPUTS_WITNESS} given twice")
            witness = True
            runner_own.add(index)
            index += 1
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
    mode = _runner_ignored_mode(only, ignored, _DECLARED_IGNORED, declared_ignored)
    if threads is None:
        threads = _runner_test_threads(environment)
    return _checked_raw(_RunnerOptions(
        binary=binary,
        filters=tuple(filters),
        skips=tuple(skips),
        exact=exact,
        declared_skips=tuple(declared_skips),
        declared_only_substrings=tuple(declared_only_substrings),
        declared_skip_substrings=tuple(declared_skip_substrings),
        declared_ignored=tuple(declared_ignored),
        project_relative_env=tuple(project_relative_env),
        project_relative_dirs=tuple(project_relative_dirs),
        fresh_dir_env=tuple(named[_FRESH_DIR_ENV]),
        clear_env=tuple(named[_CLEAR_ENV]),
        path_prefix_of_exe=prefix,
        spawn_inputs_witness=witness,
        ignored_mode=mode,
        show_output=show_output,
        test_threads=threads,
        per_test_timeout_seconds=timeout,
        raw_one_case=raw_one_case,
        list_only=False,
        list_arguments=(),
    ))
