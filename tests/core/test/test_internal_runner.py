#!/usr/bin/env fbpython
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict


from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test, env

# Empty test executor forces internal test executor to be used.
INTERNAL_TEST_EXECUTOR = ""


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_test_executor(buck: Buck) -> None:
    await buck.test(
        ":trivial_pass",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )


@buck_test()
@env("TEST_VAR", "BAD_VALUE")
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_test_executor_env(buck: Buck) -> None:
    await buck.test(
        ":check_env",
        "--",
        "--env",
        "TEST_VAR=TEST_VALUE",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_test_executor_timeout(buck: Buck) -> None:
    await expect_failure(
        buck.test(
            ":timeout",
            "--",
            "--timeout",
            "1",
            test_executor=INTERNAL_TEST_EXECUTOR,
        ),
        stderr_regex="Timeout: ",
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_provider_without_cache_permission(buck: Buck) -> None:
    await buck.test(
        "-c",
        "test.use_internal_runner=true",
        ":internal_default_pass",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_cache_request_without_root_executes_normally(buck: Buck) -> None:
    # Reuse needs an explicitly configured store; without one the case runs and nothing is
    # published.
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        ":internal_cache_request",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert "no [test] local_case_result_cache_root is configured" in result.stderr


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_cache_request_with_literal_tool_executes_normally(
    buck: Buck,
) -> None:
    # `fbpython` is found through PATH, so the command is not bound to its declared inputs:
    # the case is ineligible and executes normally instead of being reused.
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        "-c",
        f"test.local_case_result_cache_root={buck.cwd / 'local-case-store'}",
        ":internal_cache_request",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert "argv[0] `fbpython` is found through the working directory or PATH" in result.stderr


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_runner_disabled_runs_external(buck: Buck) -> None:
    # The executor spec and its expansion both come from the External provider: its command
    # runs with its own environment, and the Internal-only `INTERNAL_ONLY` is neither a
    # dangling handle nor set.
    await buck.test(
        "-c",
        "test.use_internal_runner=false",
        "-c",
        "provider_pair.expected=external",
        ":provider_pair",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_unselected_framework_runs_external(buck: Buck) -> None:
    await buck.test(
        "-c",
        "test.use_internal_runner=rust",
        "-c",
        "provider_pair.expected=external",
        ":provider_pair",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_runner_enabled_runs_internal(buck: Buck) -> None:
    # The fixture's External command fails when Internal is expected, so passing proves the
    # Internal provider ran.
    await buck.test(
        "-c",
        "test.use_internal_runner=true",
        "-c",
        "provider_pair.expected=internal",
        ":provider_pair",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_internal_only_provider_with_runner_disabled_is_refused(buck: Buck) -> None:
    await expect_failure(
        buck.test(
            "-c",
            "test.use_internal_runner=false",
            ":internal_default_pass",
            test_executor=INTERNAL_TEST_EXECUTOR,
        ),
        stderr_regex="does not select test type `custom`, and the target has no `ExternalRunnerTestInfo`",
    )


FRESH_DIRS_ONE_PASS = (
    "Tests finished: Pass 1. Fail 0. Timeout 0. Fatal 0. Skip 0. Omit 0. "
    "Infra Failure 0. Build failure 0"
)


def _assert_one_fresh_dirs_pass(stderr: str) -> None:
    # Exactly one passing result: a run that reports no result, or any other count, fails.
    assert FRESH_DIRS_ONE_PASS in stderr, stderr
    assert "NO TESTS RAN" not in stderr, stderr


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_gives_each_run_new_empty_directories(buck: Buck) -> None:
    # The case fails unless TMPDIR holds exactly its two declared directories, each empty and
    # owner-only, and it leaves files in them; so the second run passes only if they were
    # removed and recreated before it.
    for _ in range(2):
        result = await buck.test(
            "-c",
            "test.use_internal_runner=true",
            ":internal_fresh_dirs",
            test_executor=INTERNAL_TEST_EXECUTOR,
        )
        _assert_one_fresh_dirs_pass(result.stderr)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_concurrent_runs_of_a_case_do_not_share_directories(
    buck: Buck,
) -> None:
    # Each run holds its files for a while before checking them; a concurrent run of the same
    # case that removed or reused the case's directories in between would fail one of them.
    import asyncio

    results = await asyncio.gather(
        *(
            buck.test(
                "-c",
                "test.use_internal_runner=true",
                "-c",
                "fresh_dirs.hold=3",
                ":internal_fresh_dirs",
                test_executor=INTERNAL_TEST_EXECUTOR,
            )
            for _ in range(2)
        )
    )
    for result in results:
        _assert_one_fresh_dirs_pass(result.stderr)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_no_result_control(buck: Buck) -> None:
    # The assertion rejects a run that reports no result.
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        ":internal_fresh_dirs",
        "--include",
        "no_such_label",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    try:
        _assert_one_fresh_dirs_pass(result.stderr)
    except AssertionError:
        return
    raise AssertionError("a run without results was accepted")
