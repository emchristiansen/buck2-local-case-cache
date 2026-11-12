"""Add one project-relative variable name or directory to the ones the runner collects."""

from libtest_process_runner.option_value import option_value as _value
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError

_PROJECT_RELATIVE_ENV = "--project-relative-env"
_PROJECT_RELATIVE_DIR = "--project-relative-dir"


def relative_env_option(arguments: list[str], index: int, names: list[str]) -> int:
    # Returns the index of the argument after the name.
    name, index = _value(arguments, index, _PROJECT_RELATIVE_ENV)
    if not name or "=" in name:
        raise _RunnerUsageError(
            f"{_PROJECT_RELATIVE_ENV} needs a variable name, got {name!r}"
        )
    if name in names:
        raise _RunnerUsageError(f"{_PROJECT_RELATIVE_ENV} names {name!r} twice")
    names.append(name)
    return index


def relative_dir_option(arguments: list[str], index: int, paths: list[str]) -> int:
    # Returns the index of the argument after the path.
    path, index = _value(arguments, index, _PROJECT_RELATIVE_DIR)
    if not path or path in paths:
        raise _RunnerUsageError(f"{_PROJECT_RELATIVE_DIR} needs a new path, got {path!r}")
    paths.append(path)
    return index
