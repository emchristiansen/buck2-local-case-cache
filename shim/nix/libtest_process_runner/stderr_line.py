"""Write one complete line to the runner's stderr, never mixed with a line of another case.

Cases run on several threads, and a reader such as the route's spawn-inputs witness
check finds a record only at the start of a line. So every runner line that may be
written while cases run comes here: under one lock, the line and its newline go to the
stream's byte buffer in one write, which is then flushed. A concurrent line therefore
lands wholly before or wholly after it, never inside it.
"""

import sys as _sys
import threading as _threading

_WRITE = _threading.Lock()


def write_stderr_line(line: str | bytes) -> None:
    """Write `line` and a newline to stderr in one write; text is encoded as `print` would."""
    stream = _sys.stderr
    data = line if isinstance(line, bytes) else line.encode(stream.encoding, stream.errors)
    with _WRITE:
        # Anything already written as text goes first, so the order is kept.
        stream.flush()
        stream.buffer.write(data + b"\n")
        stream.buffer.flush()
