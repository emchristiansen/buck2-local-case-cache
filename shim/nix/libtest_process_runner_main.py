#!/usr/bin/env python3
"""Entry point of the per-test process runner that wraps every Buck2 Rust test command."""

import sys

from libtest_process_runner.run_tests_in_processes import run_tests_in_processes

if __name__ == "__main__":
    sys.exit(run_tests_in_processes(sys.argv[1:]))
