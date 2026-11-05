# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load(":case_parsers.bzl", "parse_internal_listing", "parse_internal_result")

def _impl(ctx):
    if ctx.attrs.internal:
        return [
            DefaultInfo(),
            InternalRunnerTestInfo(
                type = "custom",
                command = ["fbpython", "-c", ctx.attrs.script],
                # Listing gets no fresh directories.
                listing_command = [
                    "fbpython",
                    "-c",
                    "import os; assert not any(n in os.environ for n in {}); print('case')".format(ctx.attrs.fresh_dir_env),
                ],
                parse_test_listing = parse_internal_listing,
                parse_test_result = parse_internal_result,
                supports_test_execution_caching = ctx.attrs.local_case_cache,
                fresh_dir_env = ctx.attrs.fresh_dir_env,
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
        "fresh_dir_env": attrs.list(attrs.string(), default = []),
        "internal": attrs.bool(default = False),
        "local_case_cache": attrs.bool(default = False),
        "script": attrs.string(),
    },
)

# Each provider's command passes only when that provider is the expected one and runs with its own
# environment. Only the Internal provider declares `INTERNAL_ONLY`, so running the wrong provider,
# or pairing one provider's command or environment with the other's, fails.
_PAIR_SCRIPT = "import os, sys; sys.exit(0 if '{provider}' == '{expected}' and os.environ.get('{own}') == '1' and '{other}' not in os.environ else 1)"

def _pair_command(provider, expected, own, other):
    return [
        "fbpython",
        "-c",
        _PAIR_SCRIPT.format(provider = provider, expected = expected, own = own, other = other),
    ]

def _provider_pair_impl(ctx):
    expected = ctx.attrs.expected
    return [
        DefaultInfo(),
        InternalRunnerTestInfo(
            type = "custom",
            command = _pair_command("internal", expected, "INTERNAL_ONLY", "EXTERNAL_ONLY"),
            env = {"INTERNAL_ONLY": "1"},
            listing_command = ["fbpython", "-c", "print('case')"],
            parse_test_listing = parse_internal_listing,
            parse_test_result = parse_internal_result,
        ),
        ExternalRunnerTestInfo(
            command = _pair_command("external", expected, "EXTERNAL_ONLY", "INTERNAL_ONLY"),
            env = {"EXTERNAL_ONLY": "1"},
            type = "custom",
        ),
    ]

provider_pair_test = rule(
    impl = _provider_pair_impl,
    attrs = {
        # `internal` or `external`: the provider the test run is expected to use.
        "expected": attrs.string(),
    },
)
