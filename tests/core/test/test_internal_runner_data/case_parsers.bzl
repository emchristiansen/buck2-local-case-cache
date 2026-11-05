# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Self-contained internal-runner parsers: a module of top-level defs that loads nothing."""

def parse_internal_listing(stdout):
    return [{"name": name, "filter": name} for name in stdout.strip().split("\n") if name]

def parse_internal_result(stdout, stderr, exit_code):
    return [{
        "name": "case",
        "status": "PASS" if exit_code == 0 else "FAIL",
        "message": None if exit_code == 0 else stderr,
    }]
