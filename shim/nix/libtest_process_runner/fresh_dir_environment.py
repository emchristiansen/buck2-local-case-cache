"""Give one test process a new, empty directory of its own for each declared variable."""

import os as _os
import tempfile as _tempfile


def fresh_dir_environment(
    environment: dict[bytes, bytes], names: tuple[str, ...]
) -> dict[bytes, bytes]:
    # Each directory is made, owner-only, inside the run's temporary directory, which
    # TMPDIR names here and the runner removes with everything in it when the run ends.
    # A value the test would otherwise inherit is replaced. The name is eight random
    # characters and no more: TMUX_TMPDIR, for example, names the directory a tmux
    # server puts its socket under, and a socket path must fit AF_UNIX's 108 bytes.
    fresh = dict(environment)
    for name in names:
        fresh[_os.fsencode(name)] = _tempfile.mkdtemp(prefix=b"", dir=environment[b"TMPDIR"])
    return fresh
