"""Put the directory of one given executable first on the PATH of each process the runner starts."""

import os as _os
import stat as _stat

from libtest_process_runner.constants import PATH_PREFIX_OF_EXE as _PATH_PREFIX_OF_EXE
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def with_path_prefix(
    environment: dict[bytes, bytes], options: _RunnerOptions
) -> dict[bytes, bytes]:
    # Another option that sets PATH would contradict the prefix; a project-relative
    # directory names no variable, so it cannot. The path is used as given, never
    # resolving a symbolic link. An empty or absent inherited PATH gets no empty
    # element, which would search the working directory.
    path = options.path_prefix_of_exe
    if path is None:
        return environment
    for flag, names in (
        ("--project-relative-env", options.project_relative_env),
        ("--fresh-dir-env", options.fresh_dir_env),
        ("--clear-env", options.clear_env),
    ):
        if "PATH" in names:
            raise _RunnerUsageError(f"{flag} and {_PATH_PREFIX_OF_EXE} name ['PATH']")
    if not _os.path.isabs(path):
        raise _RunnerUsageError(f"{_PATH_PREFIX_OF_EXE} needs an absolute path, got {path!r}")
    try:
        mode = _os.stat(path).st_mode
    except OSError as error:
        raise _RunnerUsageError(f"{_PATH_PREFIX_OF_EXE} cannot use {path!r}: {error}") from error
    if not _stat.S_ISREG(mode) or not mode & 0o111:
        raise _RunnerUsageError(f"{_PATH_PREFIX_OF_EXE} needs an executable file, got {path!r}")
    directory = _os.fsencode(_os.path.dirname(path))
    inherited = environment.get(b"PATH")
    return {**environment, b"PATH": directory + b":" + inherited if inherited else directory}
