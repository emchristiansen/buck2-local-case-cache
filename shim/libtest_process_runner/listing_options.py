"""The runner options of a `--list` invocation, which delegates to the binary's own listing."""

from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions


def listing_options(
    binary: str,
    timeout: float | None,
    project_relative_env: tuple[str, ...],
    project_relative_dirs: tuple[str, ...],
    clear_env: tuple[str, ...],
    path_prefix_of_exe: str | None,
    list_arguments: tuple[str, ...],
) -> _RunnerOptions:
    # A listing runs no test, so it selects nothing and no test process gets a fresh
    # directory; the project-relative variables and directories, the cleared
    # variables and the PATH prefix before `--list` apply.
    return _RunnerOptions(
        binary=binary,
        filters=(),
        skips=(),
        exact=False,
        declared_skips=(),
        declared_only_substrings=(),
        declared_skip_substrings=(),
        project_relative_env=project_relative_env,
        project_relative_dirs=project_relative_dirs,
        fresh_dir_env=(),
        clear_env=clear_env,
        path_prefix_of_exe=path_prefix_of_exe,
        ignored_mode=_IgnoredMode.SKIP,
        show_output=False,
        test_threads=1,
        per_test_timeout_seconds=timeout,
        raw_one_case=False,
        list_only=True,
        list_arguments=list_arguments,
    )
