"""The environment block this process was started with, byte for byte."""

from pathlib import Path as _Path


def initial_environment() -> dict[bytes, bytes]:
    # The interpreter may change `os.environ` before any wrapper code runs: with a C
    # locale it sets LC_CTYPE=C.UTF-8 (PEP 538). The kernel keeps the block the
    # process was executed with, so a wrapper hands on exactly what it received.
    environment = {}
    for entry in _Path("/proc/self/environ").read_bytes().split(b"\0"):
        if entry:
            name, separator, value = entry.partition(b"=")
            if not separator:
                raise ValueError(f"malformed environment entry without '=': {name!r}")
            environment[name] = value
    return environment
