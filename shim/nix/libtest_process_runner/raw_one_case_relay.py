"""Relay a witnessed raw case's standard output to the runner's own, byte for byte, and keep a copy.

Each chunk the test writes reaches the runner's standard output as it arrives, unchanged,
and nothing is added. Once the runner has stopped the test's process group it calls `stop`,
which counts, at that moment, the bytes read so far plus those waiting in the pipe. The relay
then relays exactly up to that count and ends, even if a process that left the group still
holds the pipe open or keeps writing: what such a process writes after the stop is not
relayed, however busy the pipe is.

The relay writes to the runner's standard output, so it can only go as fast as that sink
reads: a reader that stops reading holds the relay, and `stop` waits for it, because
discarding bytes would break the byte-for-byte contract. A sink that fails (a closed pipe,
any write error) ends the relay at once; `stop` then returns the error, and the case can
never pass.
"""

import fcntl as _fcntl
import os as _os
import select as _select
import struct as _struct
import sys as _sys
import termios as _termios
import threading as _threading


class RawOneCaseRelay:
    def __init__(self, source: int) -> None:
        self._source = source
        self._stop_read, self._stop_write = _os.pipe()
        self._copy: list[bytes] = []
        self._error: OSError | None = None
        # Bytes read from the pipe, and the count that ends the relay once stopped; a read and
        # its count happen under the lock, so `stop` counts exactly what it reads.
        self._read = 0
        self._limit: int | None = None
        self._lock = _threading.Lock()
        self._thread = _threading.Thread(target=self._relay)
        self._thread.start()

    def stop(self) -> tuple[bytes, OSError | None]:
        """End the relay as described above: the bytes relayed, and the sink's error if any."""
        with self._lock:
            waiting = _fcntl.ioctl(self._source, _termios.FIONREAD, b"\0\0\0\0")
            self._limit = self._read + _struct.unpack("i", waiting)[0]
        _os.write(self._stop_write, b"x")
        self._thread.join()
        for descriptor in (self._stop_read, self._stop_write):
            _os.close(descriptor)
        return b"".join(self._copy), self._error

    def _relay(self) -> None:
        try:
            while True:
                ready, _, _ = _select.select([self._source, self._stop_read], [], [])
                if self._stop_read in ready:
                    break
                if not self._forward(65536):
                    return
            while self._forward(65536):
                pass
        except OSError as error:
            self._error = error

    def _forward(self, most: int) -> int:
        # How many bytes were forwarded; none is the end of the output, or of what the stop
        # counted, which a read already under way when `stop` ran never passes.
        with self._lock:
            if self._limit is not None:
                most = min(most, self._limit - self._read)
            chunk = _os.read(self._source, min(most, 65536)) if most > 0 else b""
            self._read += len(chunk)
        if chunk:
            self._copy.append(chunk)
            _sys.stdout.buffer.write(chunk)
            _sys.stdout.buffer.flush()
        return len(chunk)
