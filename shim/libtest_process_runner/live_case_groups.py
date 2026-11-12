"""The process groups of running tests, so a signal to the runner reaches every one of them."""

import os as _os
import subprocess as _subprocess
import threading as _threading
from collections.abc import Callable as _Callable


class LiveCaseGroups:
    def __init__(self) -> None:
        # Reentrant: the signal handler runs on the main thread, which may hold it.
        self._lock = _threading.RLock()
        self._groups: set[int] = set()
        self._stop_signal: int | None = None

    def launch(self, start: _Callable[[], _subprocess.Popen]) -> _subprocess.Popen | int:
        # Returns the started process, or the signal that stopped the run. Starting
        # under the lock means no test can begin after `stop_all` has run.
        with self._lock:
            if self._stop_signal is not None:
                return self._stop_signal
            process = start()
            self._groups.add(process.pid)
            return process

    def finish(self, process: _subprocess.Popen) -> None:
        with self._lock:
            self._groups.remove(process.pid)

    def stop_all(self, signal_number: int) -> None:
        with self._lock:
            if self._stop_signal is None:
                self._stop_signal = signal_number
            for group in self._groups:
                try:
                    _os.killpg(group, signal_number)
                except ProcessLookupError:
                    # Every member of this group has already exited.
                    pass

    def stop_signal(self) -> int | None:
        with self._lock:
            return self._stop_signal
