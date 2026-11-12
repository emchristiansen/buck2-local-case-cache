"""Select the tests one runner invocation runs, exactly as libtest would select them."""

from libtest_process_runner.case_selection import CaseSelection as _CaseSelection
from libtest_process_runner.ignored_mode import IgnoredMode as _IgnoredMode
from libtest_process_runner.list_tests import list_tests as _list_tests
from libtest_process_runner.listing_error import ListingError as _ListingError
from libtest_process_runner.runner_options import RunnerOptions as _RunnerOptions


def _skip_words_keep(options: _RunnerOptions, name: str) -> bool:
    return not any(word in name for word in options.declared_skip_substrings)


def _declared_keeps(options: _RunnerOptions, name: str) -> bool:
    if options.declared_only_substrings and not any(
        word in name for word in options.declared_only_substrings
    ):
        return False
    return _skip_words_keep(options, name)


def select_cases(options: _RunnerOptions, environment: dict[bytes, bytes]) -> _CaseSelection:
    selection = [*options.filters, *(["--exact"] if options.exact else [])]
    for skip in options.skips:
        selection += ["--skip", skip]
    listed = set(_list_tests(options.binary, [], environment))
    total = len(listed)
    unknown = [name for name in options.declared_skips if name not in listed]
    if unknown:
        raise _ListingError(f"--declared-skip names no test the binary lists: {unknown!r}")
    for flag, words in (
        ("--declared-only-substring", options.declared_only_substrings),
        ("--declared-skip-substring", options.declared_skip_substrings),
    ):
        unmatched = [word for word in words if not any(word in name for name in listed)]
        if unmatched:
            raise _ListingError(f"{flag} matches no test the binary lists: {unmatched!r}")
    # Every declared word must keep a listed test once the skip words apply, so a
    # declaration cannot turn a target into a run of no tests that reports ok.
    dead = [
        word
        for word in options.declared_only_substrings
        if not any(word in name and _skip_words_keep(options, name) for name in listed)
    ]
    if dead:
        raise _ListingError(
            f"--declared-only-substring {dead[0]!r} selects no test the declared skip words leave"
        )
    if (
        options.declared_skip_substrings
        and not options.declared_only_substrings
        and not any(_skip_words_keep(options, name) for name in listed)
    ):
        raise _ListingError("--declared-skip-substring words leave no listed test")
    declared = set(options.declared_skips)
    if options.ignored_mode is _IgnoredMode.ONLY:
        to_run, reported_ignored = (
            _list_tests(options.binary, [*selection, "--ignored"], environment),
            [],
        )
    elif options.ignored_mode is _IgnoredMode.INCLUDE:
        to_run, reported_ignored = _list_tests(options.binary, selection, environment), []
    else:
        selected = _list_tests(options.binary, selection, environment)
        ignored = set(_list_tests(options.binary, [*selection, "--ignored"], environment))
        if not ignored <= set(selected):
            raise _ListingError("the ignored listing names tests the full listing does not")
        to_run = [name for name in selected if name not in ignored]
        reported_ignored = [name for name in selected if name in ignored]
    # A declared selection narrows the caller's as a filter would, so the tests it
    # leaves out count as filtered out.
    to_run = [name for name in to_run if name not in declared and _declared_keeps(options, name)]
    reported_ignored = [
        name for name in reported_ignored if name not in declared and _declared_keeps(options, name)
    ]
    filtered_out = total - len(to_run) - len(reported_ignored)
    if filtered_out < 0:
        raise _ListingError("the filtered listing names more tests than the binary has")
    return _CaseSelection(tuple(to_run), tuple(reported_ignored), filtered_out)
