# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Self-contained internal-runner parsers: a module of top-level defs that loads nothing."""

def parse_internal_listing(stdout):
    return [{"filter": name, "name": name} for name in stdout.strip().split("\n") if name]

def parse_internal_result(stdout, stderr, exit_code):
    return [
        {
            "message": None if exit_code == 0 else stderr,
            "name": "case",
            "status": "PASS" if exit_code == 0 else "FAIL",
        }
    ]

def parse_named_result(stdout, stderr, exit_code):
    """A case prints its own name; a case printing `UNPARSEABLE` makes the parser fail."""
    name = stdout.strip()
    if name == "UNPARSEABLE":
        fail("unparseable case output")
    return [
        {
            "details": "" if exit_code == 0 else stderr,
            "message": None if exit_code == 0 else stderr,
            "name": name,
            "status": "PASS" if exit_code == 0 else "FAIL",
        }
    ]

def parse_internal_result_with_details(stdout, stderr, exit_code):
    """As `parse_internal_result`, also reporting a failed case's stderr as its details."""
    return [
        {
            "details": "" if exit_code == 0 else stderr,
            "message": None if exit_code == 0 else stderr,
            "name": "case",
            "status": "PASS" if exit_code == 0 else "FAIL",
        }
    ]
