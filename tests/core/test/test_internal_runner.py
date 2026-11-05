#!/usr/bin/env fbpython
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict


import asyncio
import json
import os
import re
import signal
from pathlib import Path

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.api.buck_result import BuckResult
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
    # the case is ineligible and executes normally instead of being reused. Which unbound input
    # the closure check reports first depends on the host's PATH, so any of them will do; a
    # second run is not served from the store either.
    store = buck.cwd / "local-case-store"
    for _ in range(2):
        result = await buck.test(
            "-c",
            "test.use_internal_runner=true",
            "-c",
            f"test.local_case_result_cache_root={store}",
            ":internal_cache_request",
            test_executor=INTERNAL_TEST_EXECUTOR,
        )
        assert_exactly_one_pass(result)
        stderr = _plain_stderr(result)
        assert re.search(
            r"Local case result reuse unavailable for \S*:internal_cache_request\b.*: "
            r"the command is not bound: .+; the case executes normally",
            stderr,
        ), stderr
        assert "Local test-result reuse" not in stderr, stderr
    assert not (store / "v1").exists() or not any((store / "v1").iterdir())


_ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")

_EXACTLY_ONE_PASS = (
    "Tests finished: Pass 1. Fail 0. Timeout 0. Fatal 0. Skip 0. Omit 0. "
    "Infra Failure 0. Build failure 0"
)


def _plain_stderr(result: BuckResult) -> str:
    return _ANSI_ESCAPE.sub("", result.stderr)


def assert_exactly_one_pass(result: BuckResult) -> None:
    """The run reported exactly one result, a pass: no failure, skip, omission or listing or
    build failure, and not zero or several results."""
    stderr = _plain_stderr(result)
    assert _EXACTLY_ONE_PASS in stderr, stderr
    assert "NO TESTS RAN" not in stderr, stderr


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_runner_disabled_runs_external(buck: Buck) -> None:
    # The executor spec and its expansion both come from the External provider: its command
    # runs with its own environment, and the Internal-only `INTERNAL_ONLY` is neither a
    # dangling handle nor set.
    result = await buck.test(
        "-c",
        "test.use_internal_runner=false",
        "-c",
        "provider_pair.expected=external",
        ":provider_pair",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_unselected_framework_runs_external(buck: Buck) -> None:
    result = await buck.test(
        "-c",
        "test.use_internal_runner=rust",
        "-c",
        "provider_pair.expected=external",
        ":provider_pair",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_runner_enabled_runs_internal(buck: Buck) -> None:
    # The fixture's External command fails when Internal is expected, so passing proves the
    # Internal provider ran.
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        "-c",
        "provider_pair.expected=internal",
        ":provider_pair",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_wrong_provider_fails(buck: Buck) -> None:
    # Control for the fixture: running the other provider than the expected one fails.
    await expect_failure(
        buck.test(
            "-c",
            "test.use_internal_runner=true",
            "-c",
            "provider_pair.expected=external",
            ":provider_pair",
            test_executor=INTERNAL_TEST_EXECUTOR,
        ),
    )


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_pair_labels_come_from_the_selected_provider(buck: Buck) -> None:
    result = await buck.test(
        "-c",
        "test.use_internal_runner=false",
        "-c",
        "provider_pair.expected=external",
        ":provider_pair",
        "--include",
        "external_provider",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        "-c",
        "provider_pair.expected=internal",
        ":provider_pair",
        "--include",
        "internal_provider",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_exactly_one_pass_rejects_a_run_without_results(buck: Buck) -> None:
    # With the runner disabled the External provider's labels apply, so including only the
    # Internal provider's label runs nothing; the assertion must reject that run.
    result = await buck.test(
        "-c",
        "test.use_internal_runner=false",
        "-c",
        "provider_pair.expected=external",
        ":provider_pair",
        "--include",
        "internal_provider",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert "NO TESTS RAN" in _plain_stderr(result)
    try:
        assert_exactly_one_pass(result)
    except AssertionError:
        pass
    else:
        raise AssertionError("a run without results was accepted as exactly one pass")


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_artifact_pair_runner_disabled_builds_only_external_inputs(
    buck: Buck,
) -> None:
    # The Internal provider's input action always fails: the run passes only if the build
    # builds the External provider's distinct input and not the Internal provider's.
    for runner in ["false", "rust"]:
        result = await buck.test(
            "-c",
            f"test.use_internal_runner={runner}",
            ":provider_artifact_pair",
            test_executor=INTERNAL_TEST_EXECUTOR,
        )
        assert_exactly_one_pass(result)
        assert "internal_only_input" not in _plain_stderr(result)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_provider_artifact_pair_runner_enabled_builds_internal_inputs(
    buck: Buck,
) -> None:
    for config in [["-c", "test.use_internal_runner=true"], []]:
        await expect_failure(
            buck.test(
                *config,
                ":provider_artifact_pair",
                test_executor=INTERNAL_TEST_EXECUTOR,
            ),
            stderr_regex="internal_only_input",
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
        assert_exactly_one_pass(result)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_concurrent_runs_of_a_case_do_not_share_directories(
    buck: Buck,
) -> None:
    # Each run holds its files for a while before checking them; a concurrent run of the same
    # case that removed or reused the case's directories in between would fail one of them.
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
        assert_exactly_one_pass(result)


_PASS_2 = (
    "Tests finished: Pass 2. Fail 0. Timeout 0. Fatal 0. Skip 0. Omit 0. "
    "Infra Failure 0. Build failure 0"
)


async def _without_forkserver(buck: Buck) -> None:
    # Without a forkserver the daemon spawns the child itself and nothing kills it when the
    # executing future is dropped, so a scratch path released before its child terminated
    # leaves that child running for the next holder to observe.
    await buck.kill()
    with open(buck.cwd / ".buckconfig", "a") as buckconfig:
        buckconfig.write("\n[buck2]\nforkserver = false\n")


async def _expect_forkserver(buck: Buck, present: bool) -> None:
    status = json.loads((await buck.status()).stdout)
    assert (status.get("forkserver_pid") is not None) == present, status


def _holder_pid(signals: Path) -> int:
    return int((signals / "hold" / "holder").read_text())


def _stop_if_running(signals: Path) -> None:
    # A holder that outlived its scratch path's release must not outlive the test.
    holder = signals / "hold" / "holder"
    if holder.exists():
        try:
            os.kill(_holder_pid(signals), signal.SIGKILL)
        except ProcessLookupError:
            pass


async def _wait_for(path: Path) -> None:
    for _ in range(600):
        if path.exists():
            return
        await asyncio.sleep(0.1)
    raise AssertionError(f"{path} never appeared")


def _hold_args(signals: Path, target: str, *config: str) -> list[str]:
    return [
        "-c",
        "test.use_internal_runner=true",
        "-c",
        f"fresh_dirs.signals={signals}",
        *config,
        target,
    ]


async def _cancel_holder_with_queued_run(
    buck: Buck, target: str, *config: str
) -> None:
    # The first run's case holds the scratch path with a live child; a second run of the same
    # case waits for it; the first run is then interrupted. The second run's case passes only
    # if the first run's child was gone before the scratch path was released to it.
    signals = buck.cwd / "signals"
    args = _hold_args(signals, target, *config)
    holder = await buck.test(*args, test_executor=INTERNAL_TEST_EXECUTOR).start()
    try:
        await _wait_for(signals / "hold" / "holder")
        queued = asyncio.ensure_future(
            buck.test(*args, test_executor=INTERNAL_TEST_EXECUTOR)
        )
        # Give the second run time to reach the case and wait for its scratch path; it
        # discriminates equally if it gets there only after the interruption.
        await asyncio.sleep(3)
        holder.send_signal(signal.SIGINT)
        await holder.communicate()
        assert holder.returncode != 0, holder.returncode
        result = await queued
        assert_exactly_one_pass(result)
        assert (signals / "hold" / "queued").read_text() == "gone"
    finally:
        _stop_if_running(signals)


async def _holder_beside_parser_failure(
    buck: Buck, target: str = ":fresh_hold_beside_parser_failure", *config: str
) -> None:
    # The first run ends on its sibling's unparseable output while `hold` still runs, which
    # abandons `hold`'s execution; the next run's `hold` passes only if that child was gone
    # before the scratch path was released.
    signals = buck.cwd / "signals"
    args = _hold_args(signals, target, *config)
    try:
        await expect_failure(
            buck.test(*args, test_executor=INTERNAL_TEST_EXECUTOR),
            stderr_regex="unparseable case output",
        )
        result = await buck.test(*args, test_executor=INTERNAL_TEST_EXECUTOR)
        assert _PASS_2 in _plain_stderr(result), result.stderr
        assert (signals / "hold" / "queued").read_text() == "gone"
    finally:
        _stop_if_running(signals)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_cancelled_holder_releases_after_its_child(
    buck: Buck,
) -> None:
    await _cancel_holder_with_queued_run(buck, ":fresh_hold")
    await _expect_forkserver(buck, True)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_cancelled_holder_releases_after_its_child_without_forkserver(
    buck: Buck,
) -> None:
    await _without_forkserver(buck)
    await _cancel_holder_with_queued_run(buck, ":fresh_hold")
    await _expect_forkserver(buck, False)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_cancelled_holder_without_store_releases_after_its_child(
    buck: Buck,
) -> None:
    # Reuse requested, but no store is configured: the case executes normally.
    await _without_forkserver(buck)
    await _cancel_holder_with_queued_run(buck, ":fresh_hold_cache_requested")
    await _expect_forkserver(buck, False)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_cancelled_holder_with_unsupported_parser_releases_after_its_child(
    buck: Buck,
) -> None:
    # Reuse requested with a store, but the parser cannot be keyed: the case executes
    # normally.
    await _without_forkserver(buck)
    store = buck.cwd / "local-case-store"
    await _cancel_holder_with_queued_run(
        buck,
        ":fresh_hold_unsupported_parser",
        "-c",
        f"test.local_case_result_cache_root={store}",
    )
    await _expect_forkserver(buck, False)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_holder_beside_parser_failure_releases_after_its_child(
    buck: Buck,
) -> None:
    await _holder_beside_parser_failure(buck)
    await _expect_forkserver(buck, True)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_holder_beside_parser_failure_releases_after_its_child_without_forkserver(
    buck: Buck,
) -> None:
    await _without_forkserver(buck)
    await _holder_beside_parser_failure(buck)
    await _expect_forkserver(buck, False)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_holder_without_store_beside_parser_failure_releases_after_its_child(
    buck: Buck,
) -> None:
    # Reuse requested, but no store is configured: the case executes normally.
    await _without_forkserver(buck)
    await _holder_beside_parser_failure(
        buck, ":fresh_hold_beside_parser_failure_cache_requested"
    )
    await _expect_forkserver(buck, False)


@buck_test(skip_for_os=["windows", "darwin"])
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_holder_with_unsupported_parser_beside_parser_failure_releases_after_its_child(
    buck: Buck,
) -> None:
    # Reuse requested with a store, but the parser cannot be keyed: the case executes
    # normally.
    await _without_forkserver(buck)
    store = buck.cwd / "local-case-store"
    await _holder_beside_parser_failure(
        buck,
        ":fresh_hold_beside_parser_failure_unsupported_parser",
        "-c",
        f"test.local_case_result_cache_root={store}",
    )
    await _expect_forkserver(buck, False)


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_names_are_not_replaced_by_invocation_identity(
    buck: Buck,
) -> None:
    # `BUCK_BUILD_ID` and `BUCK2_DAEMON_UUID` are otherwise set by the executor for every
    # command; declared as fresh directory names they must still be the case's directories.
    started = buck.cwd / "started.json"
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        "-c",
        f"fresh_dirs.started={started}",
        ":fresh_identity_names",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)
    assert started.exists()


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_name_set_by_a_local_resource_is_refused(
    buck: Buck,
) -> None:
    # A local resource would set the case's fresh directory variable to its own value; the
    # case is refused before its child starts.
    started = buck.cwd / "started.json"
    await expect_failure(
        buck.test(
            "-c",
            "test.use_internal_runner=true",
            "-c",
            f"fresh_dirs.started={started}",
            ":fresh_name_set_by_resource",
            test_executor=INTERNAL_TEST_EXECUTOR,
        ),
        stderr_regex="Scratch directory variable `FRESH_A` is also set by a local resource",
    )
    assert not started.exists()


@buck_test()
@env("BUCK2_ALLOW_INTERNAL_TEST_RUNNER_DO_NOT_USE", "1")
async def test_fresh_dir_env_beside_an_unrelated_local_resource(buck: Buck) -> None:
    # The control: a local resource setting another variable leaves the fresh directory in
    # place and still sets its own.
    started = buck.cwd / "started.json"
    result = await buck.test(
        "-c",
        "test.use_internal_runner=true",
        "-c",
        f"fresh_dirs.started={started}",
        ":fresh_beside_unrelated_resource",
        test_executor=INTERNAL_TEST_EXECUTOR,
    )
    assert_exactly_one_pass(result)
    assert json.loads(started.read_text())["OTHER"] == "42"
