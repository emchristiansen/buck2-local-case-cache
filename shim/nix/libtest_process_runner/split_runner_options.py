"""Split `[--per-test-timeout-seconds N] -- BINARY [libtest arguments]` at its `--`."""

from libtest_process_runner.positive_number import positive_number as _positive
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError

_TIMEOUT_OPTION = "--per-test-timeout-seconds"


def split_runner_options(argv: list[str]) -> tuple[float | None, list[str]]:
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
