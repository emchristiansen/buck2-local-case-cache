"""Which ignored tests a run selects, as libtest's flags choose them."""

from enum import Enum as _Enum


class IgnoredMode(_Enum):
    # No flag: ignored tests are reported as ignored and not run.
    SKIP = "skip"
    # `--ignored`: only ignored tests are selected, and they run.
    ONLY = "only"
    # `--include-ignored`: every selected test runs, ignored or not.
    INCLUDE = "include"
