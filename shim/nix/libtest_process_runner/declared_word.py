"""Add one runner option's nonempty, not yet given word to the words it collects."""

from libtest_process_runner.option_value import option_value as _option_value
from libtest_process_runner.runner_usage_error import RunnerUsageError as _RunnerUsageError


def declared_word(arguments: list[str], index: int, flag: str, words: list[str]) -> int:
    # Returns the index of the argument after the word.
    word, index = _option_value(arguments, index, flag)
    if not word:
        raise _RunnerUsageError(f"{flag} needs a nonempty word")
    if word in words:
        raise _RunnerUsageError(f"{flag} names {word!r} twice")
    words.append(word)
    return index
