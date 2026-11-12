"""Read the value of one runner option given as `FLAG VALUE` or `FLAG=VALUE`."""

from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def option_value(arguments: list[str], index: int, flag: str) -> tuple[str, int]:
    # Returns the value and the index of the argument after it.
    argument = arguments[index]
    if argument.startswith(flag + "="):
        return argument[len(flag) + 1 :], index + 1
    if index + 1 >= len(arguments):
        raise _RunnerUsageError(f"{flag} needs a value")
    return arguments[index + 1], index + 2
