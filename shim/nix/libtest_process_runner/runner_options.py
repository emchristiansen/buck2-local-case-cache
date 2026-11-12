"""One parsed runner invocation: the test binary and the libtest selection it asks for."""

from dataclasses import dataclass as _dataclass

from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode


@_dataclass(frozen=True)
class RunnerOptions:
    binary: str
    # Positional libtest filters, passed unchanged to listing.
    filters: tuple[str, ...]
    # `--skip` filters, passed unchanged to listing.
    skips: tuple[str, ...]
    # `--exact` applies to the filters and skips at listing.
    exact: bool
    # `--declared-skip NAME`: a target's own exact test names, never passed to libtest.
    declared_skips: tuple[str, ...]
    # `--declared-only-substring WORD`: when given, only tests whose name contains one of these
    # substrings run. Never passed to libtest, so the caller's `--exact` never applies.
    declared_only_substrings: tuple[str, ...]
    # `--declared-skip-substring WORD`: tests whose name contains one of these
    # substrings are left out. Never passed to libtest.
    declared_skip_substrings: tuple[str, ...]
    # `--declared-ignored NAME`: a target's own exact names of ignored tests, which run
    # with libtest's `--include-ignored`; every other ignored test stays reported ignored.
    declared_ignored: tuple[str, ...]
    # `--project-relative-env NAME`: variables whose project-relative path value
    # every test process and listing receives absolute.
    project_relative_env: tuple[str, ...]
    # `--project-relative-dir PATH`: directories the runner creates before any test runs.
    project_relative_dirs: tuple[str, ...]
    # `--fresh-dir-env NAME`: variables each test process receives naming a new, empty
    # directory of its own inside the run's temporary directory.
    fresh_dir_env: tuple[str, ...]
    # `--clear-env NAME`: variables removed from the environment of every process the
    # runner starts, test and listing alike.
    clear_env: tuple[str, ...]
    # `--path-prefix-of-exe PATH`: an absolute executable whose directory is put first on
    # the PATH of every process the runner starts; `None` leaves PATH as inherited.
    path_prefix_of_exe: str | None
    # `--spawn-inputs-witness`: after each case, write its final spawn inputs bound to its
    # outcome as one stderr line (spawn_inputs_witness). Off unless the route sets it.
    spawn_inputs_witness: bool
    ignored_mode: _IgnoredMode
    # `--nocapture` or `--show-output`: print passing tests' output too.
    show_output: bool
    # Test processes run at once.
    test_threads: int
    # `None` leaves each test unbounded, as libtest and `buck2 test` do.
    per_test_timeout_seconds: float | None
    # `--raw-one-case`: run the one exactly selected test as `BINARY --exact NAME` and hand
    # on its own output and exit status unchanged, with no report of the runner's own.
    raw_one_case: bool
    # `--list`: delegate to the binary's own listing and run nothing.
    list_only: bool
    # Arguments for `--list`, in their original order.
    list_arguments: tuple[str, ...]
