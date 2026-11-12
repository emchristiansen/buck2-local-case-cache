"""List a libtest binary's tests with `--list --format terse`."""

import re as _re
import subprocess as _subprocess

from libtest_process_runner.constants import LIBTEST_LISTING_PATTERN as _LISTING_PATTERN
from libtest_process_runner.listing_error import ListingError as _ListingError

_LISTING_LINE = _re.compile(_LISTING_PATTERN)


def list_tests(binary: str, arguments: list[str], environment: dict[bytes, bytes]) -> list[str]:
    command = [binary, "--list", "--format", "terse", *arguments]
    completed = _subprocess.run(
        command, env=environment, stdin=_subprocess.DEVNULL, capture_output=True, check=False
    )
    if completed.returncode != 0:
        raise _ListingError(
            f"{command!r} exited with {completed.returncode}:\n"
            + completed.stdout.decode(errors="replace")
            + completed.stderr.decode(errors="replace")
        )
    names = []
    for line in completed.stdout.decode().splitlines():
        if not line:
            continue
        matched = _LISTING_LINE.match(line)
        if matched is None:
            raise _ListingError(f"{command!r} printed a line that is not a test listing: {line!r}")
        names.append(matched["name"])
    if len(set(names)) != len(names):
        raise _ListingError(f"{command!r} listed a test name more than once")
    return names
