# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

def _parse_internal_listing(stdout):
    return [{"name": name, "filter": name} for name in stdout.strip().split("\n") if name]

def _parse_internal_result(stdout, stderr, exit_code):
    return [{
        "name": "case",
        "status": "PASS" if exit_code == 0 else "FAIL",
        "message": None if exit_code == 0 else stderr,
    }]

def _impl(ctx):
    if ctx.attrs.internal:
        listing_script = (
            "raise AssertionError('strict listing child was launched')"
            if ctx.attrs.local_case_cache
            else "print('case')"
        )
        return [
            DefaultInfo(),
            InternalRunnerTestInfo(
                type = "custom",
                command = ["fbpython", "-c", ctx.attrs.script],
                listing_command = ["fbpython", "-c", listing_script],
                parse_test_listing = _parse_internal_listing,
                parse_test_result = _parse_internal_result,
                supports_test_execution_caching = ctx.attrs.local_case_cache,
            ),
        ]
    return [
        DefaultInfo(),
        ExternalRunnerTestInfo(
            command = ["fbpython", "-c", ctx.attrs.script],
            type = "custom",
        ),
    ]

python_test = rule(
    impl = _impl,
    attrs = {
        "internal": attrs.bool(default = False),
        "local_case_cache": attrs.bool(default = False),
        "script": attrs.string(),
    },
)
