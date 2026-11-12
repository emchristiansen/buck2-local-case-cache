/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! In-process test runner for `InternalRunnerTestInfo`.
//!
//! Drives the listing → parse_test_listing callback → per-test execution flow
//! entirely within the Buck2 process, using the `TestOrchestrator` trait
//! directly (no gRPC, no subprocess).

use std::time::Duration;

use async_trait::async_trait;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::CaseResultReuse;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::InternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::OwnedInternalRunnerTestInfo;
use buck2_error::BuckErrorContext;
use buck2_hash::BuckFixedSortedMap;
use buck2_test_api::data::ArgValue;
use buck2_test_api::data::ArgValueContent;
use buck2_test_api::data::ConfiguredTargetHandle;
use buck2_test_api::data::ExecuteResponse;
use buck2_test_api::data::ExecutionStatus;
use buck2_test_api::data::ExecutionStream;
use buck2_test_api::data::ExternalRunnerSpec;
use buck2_test_api::data::ExternalRunnerSpecValue;
use buck2_test_api::data::LocalResourceType;
use buck2_test_api::data::RemoteCacheAnswer;
use buck2_test_api::data::RequiredLocalResources;
use buck2_test_api::data::TestResult;
use buck2_test_api::data::TestResultEntry;
use buck2_test_api::data::TestResultOrigin;
use buck2_test_api::data::TestStage;
use buck2_test_api::data::TestStatus;
use buck2_test_api::protocol::TestOrchestrator;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use host_sharing::HostSharingRequirements;

use crate::local_result_cache::ConfirmedPass;
use crate::local_result_cache::DiscoveryIdentity;
use crate::local_result_cache::LocalCaseGap;
use crate::local_result_cache::LocalCaseRequest;
use crate::local_result_cache::LocalCaseOutcome;
use crate::local_result_cache::LocalCaseResult;
use crate::local_result_cache::reject_reused;
use crate::local_result_cache::settle_fresh;
use crate::local_result_cache::settle_unconfirmed;

/// The in-process orchestrator interface, which can also return a case's local reuse or
/// captured success for confirmation by this runner.
#[async_trait]
pub(crate) trait InternalTestOrchestrator: TestOrchestrator {
    async fn execute_case(
        &self,
        stage: TestStage,
        test_target: ConfiguredTargetHandle,
        cmd: Vec<ArgValue>,
        env: BuckFixedSortedMap<String, ArgValue>,
        timeout: Duration,
        required_local_resources: RequiredLocalResources,
        disable_test_execution_caching: bool,
        local_case: Option<LocalCaseRequest>,
    ) -> buck2_error::Result<(ExecuteResponse, LocalCaseOutcome)>;
}

/// Run the full test lifecycle for a single target in-process:
/// 1. Execute the test binary in listing mode
/// 2. Parse listing output via the Starlark `parse_test_listing` callback
/// 3. Report discovered tests
/// 4. Execute each discovered test individually
/// 5. Report each test result
///
/// When the provider requests local case reuse, each case's request carries the discovery
/// identity, and a captured success is published only after its parsed result is exactly one PASS
/// for that case and was reported. A reused result is reported only if the current parser
/// confirms it the same way; otherwise it is invalidated and the case executes again. A tracked
/// fresh case whose parse is not exactly one result for it is reported as an explicit FAIL,
/// never as an ordinary PASS.
pub async fn run_internal_test(
    orchestrator: &dyn InternalTestOrchestrator,
    spec: ExternalRunnerSpec,
    listing_spec: ExternalRunnerSpec,
    provider: &OwnedInternalRunnerTestInfo,
    timeout: Duration,
) -> buck2_error::Result<()> {
    let target_handle = spec.target.handle;
    let suite = spec.target.target.clone();

    // `'v`-branded views of the provider must not be held across awaits (only the `OwnedFrozen`
    // may be), so views are derived in scopes that end before the next await.
    let (required_local_resources, case_result_reuse, test_type, parsers) = {
        let provider = provider.as_ref().value().as_ref();
        (
            RequiredLocalResources {
                resources: provider
                    .required_local_resources()
                    .map(|r| LocalResourceType {
                        name: r.name.clone(),
                    })
                    .collect(),
            },
            provider.case_result_reuse(),
            provider.test_type().to_owned(),
            provider.self_contained_parser_identity(),
        )
    };

    // Step 1: Execute listing using the dedicated listing command
    let listing_command = build_command_from_spec(&listing_spec);
    let listing_stage = TestStage::Listing {
        suite: suite.clone(),
        cacheable: true,
    };

    let listing_response = orchestrator
        .execute2(
            listing_stage,
            target_handle,
            listing_command,
            build_env_from_spec(&listing_spec),
            timeout,
            HostSharingRequirements::default(),
            Vec::new(),
            None,
            required_local_resources.clone(),
            false,
        )
        .await
        .buck_error_context("Listing execution failed")?;

    let listing_result = match listing_response {
        ExecuteResponse::Result(r) => r,
        ExecuteResponse::Cancelled(_) => return Ok(()),
    };

    // Check listing exit status before parsing
    match &listing_result.status {
        ExecutionStatus::Finished { exitcode } if *exitcode != 0 => {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Test listing exited with code {}",
                exitcode
            ));
        }
        ExecutionStatus::TimedOut { .. } => {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Test listing timed out"
            ));
        }
        _ => {}
    }

    let listing_bytes = match &listing_result.stdout {
        ExecutionStream::Inline(bytes) => bytes,
    };
    let listing_output = String::from_utf8_lossy(listing_bytes).to_string();

    // Step 2: Parse listing output via Starlark callback
    let discovered_tests =
        InternalRunnerTestInfo::parse_test_listing_output(provider, &listing_output)
            .buck_error_context("Failed to parse test listing output")?;

    let requests_local_case_reuse = case_result_reuse == CaseResultReuse::HostStore;
    let remote_reuse = case_result_reuse == CaseResultReuse::RemoteActionCache;
    let discovery = requests_local_case_reuse.then(|| match &parsers {
        Ok(parsers) => Ok(DiscoveryIdentity::new(
            &test_type,
            &spec_identity(&spec),
            &spec_identity(&listing_spec),
            parsers,
            listing_bytes,
        )),
        Err(reason) => {
            tracing::warn!(
                "Local case result reuse unavailable for {}: {}",
                suite,
                reason
            );
            Err(LocalCaseGap::ParserNotSelfContained)
        }
    });

    // Step 3: Report discovered tests
    let test_names: Vec<String> = discovered_tests.iter().map(|t| t.name.clone()).collect();
    orchestrator
        .report_tests_discovered(target_handle, suite.clone(), test_names)
        .await
        .buck_error_context("Failed to report discovered tests")?;

    // Step 4+5: Execute and report each discovered test.
    // FuturesUnordered pipelines execute calls so the next test's orchestrator
    // dispatch overlaps with result parsing of the previous test.
    let run_case = |entry_name: String, filter: String, reuse: bool| {
        let test_stage = TestStage::Testing {
            suite: suite.clone(),
            testcases: vec![filter.clone()],
            variant: None,
            repeat_count: None,
        };
        let mut test_command = build_command_from_spec(&spec);
        test_command.push(ArgValue {
            content: ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::Verbatim(
                filter.clone(),
            )),
            format: None,
        });
        let env = build_env_from_spec(&spec);
        let required_local_resources = required_local_resources.clone();
        let local_case = discovery.as_ref().map(|discovery| match discovery {
            Ok(discovery) => LocalCaseRequest::Track {
                discovery: discovery.clone(),
                reuse,
            },
            Err(gap) => LocalCaseRequest::Unsupported(gap.clone()),
        });
        async move {
            let test_response = orchestrator
                .execute_case(
                    test_stage,
                    target_handle,
                    test_command,
                    env,
                    timeout,
                    required_local_resources,
                    // A case whose remote action-cache hit was not confirmed executes again
                    // remotely without reading any cache.
                    remote_reuse && !reuse,
                    local_case,
                )
                .await;
            (entry_name, filter, reuse, test_response)
        }
    };

    let mut futures = FuturesUnordered::new();
    for entry in &discovered_tests {
        futures.push(run_case(entry.name.clone(), entry.filter.clone(), true));
    }

    while let Some((entry_name, filter, reuse, test_response)) = futures.next().await {
        match test_response {
            Ok((ExecuteResponse::Result(result), local_case)) => {
                let local_case = local_case.take();
                // A cached remote answer ran no case for this request: an action-cache hit, or
                // an execute request the remote service served from its own cache or
                // deduplicated. On a first run it is reported only once the current parser
                // confirms it.
                let cached_answer = remote_reuse
                    .then(|| result.remote_cached_answer().map(|(_, answer)| answer))
                    .flatten();
                match (reuse, cached_answer) {
                    (false, Some(RemoteCacheAnswer::Deduplicated)) => {
                        // A cache-bypassing re-execution the service deduplicated against an
                        // identical execution already in flight: a real run that read no cache.
                        // It is reported strictly below as this run's execution, never as reuse.
                        tracing::debug!(
                            "Cache-bypassing execution of {} was deduplicated; reporting it \
                             strictly",
                            entry_name
                        );
                    }
                    (false, Some(answer)) => {
                        // A cache-bypassing re-execution must run the case, so a cached answer to
                        // it is never trusted, whatever it says.
                        let test_result = TestResult {
                            target: target_handle,
                            name: entry_name,
                            status: TestStatus::FATAL,
                            msg: Some(format!(
                                "The remote service answered a cache-bypassing execution of this \
                                 case from its cache ({answer:?})"
                            )),
                            duration: Some(result.execution_time),
                            details: format_execution_output(&result.stdout, &result.stderr),
                            max_memory_used_bytes: result.max_memory_used_bytes,
                            origin: TestResultOrigin::of_execution(&result),
                        };
                        orchestrator
                            .report_test_result(test_result)
                            .await
                            .buck_error_context("Failed to report test result")?;
                        continue;
                    }
                    _ => {}
                }
                // Only a first run's cached answer is reuse to confirm; a deduplicated bypass
                // run is this run's own strict execution.
                let remote_hit = reuse && cached_answer.is_some();
                let origin = if remote_reuse && !remote_hit {
                    TestResultOrigin::Fresh
                } else {
                    TestResultOrigin::of_execution(&result)
                };
                // A fresh generation recorded for an eligible case, successful or not, or a
                // fresh execution of a case that reuses remote results: both are reported
                // strictly.
                let tracked_fresh = matches!(
                    local_case,
                    Some(LocalCaseResult::Captured(_) | LocalCaseResult::Unsuccessful)
                ) || (remote_reuse && !remote_hit);
                let exit_code = match result.status {
                    ExecutionStatus::Finished { exitcode } => exitcode,
                    ExecutionStatus::TimedOut { .. } if remote_hit => {
                        // A cached answer that timed out is not a confirmed PASS for this case:
                        // execute the case again without reading any cache.
                        futures.push(run_case(entry_name, filter, false));
                        continue;
                    }
                    ExecutionStatus::TimedOut { .. } => {
                        settle_unconfirmed(local_case, "the case timed out").await;
                        // For timeouts, report immediately without calling the callback
                        let test_result = TestResult {
                            target: target_handle,
                            name: entry_name,
                            status: TestStatus::TIMEOUT,
                            msg: Some("Test timed out".to_owned()),
                            duration: Some(result.execution_time),
                            details: format_execution_output(&result.stdout, &result.stderr),
                            max_memory_used_bytes: result.max_memory_used_bytes,
                            origin: TestResultOrigin::Fresh,
                        };
                        orchestrator
                            .report_test_result(test_result)
                            .await
                            .buck_error_context("Failed to report test result")?;
                        continue;
                    }
                };

                let stdout_str = match &result.stdout {
                    ExecutionStream::Inline(bytes) => String::from_utf8_lossy(bytes).to_string(),
                };
                let stderr_str = match &result.stderr {
                    ExecutionStream::Inline(bytes) => String::from_utf8_lossy(bytes).to_string(),
                };

                let parsed = InternalRunnerTestInfo::parse_test_result_output(
                    provider,
                    &stdout_str,
                    &stderr_str,
                    exit_code,
                );
                if remote_hit {
                    // The remote worker publishes a result by its exit code alone, so the entry
                    // may hold a failed, ignored, partial or malformed run: anything the current
                    // parser does not confirm as one PASS for this case executes again.
                    let confirmed = parsed
                        .map_err(|e| format!("the parser failed: {e:#}"))
                        .and_then(
                            |entries| match confirm_case(&entry_name, exit_code, &entries) {
                                Ok(_) => Ok(entries),
                                Err(reason) => Err(reason.to_string()),
                            },
                        );
                    match confirmed {
                        Ok(result_entries) => {
                            report_entries(
                                orchestrator,
                                target_handle,
                                &entry_name,
                                exit_code,
                                &result,
                                result_entries,
                                origin.clone(),
                                false,
                            )
                            .await?;
                        }
                        Err(reason) => {
                            tracing::debug!(
                                "Remote cached result for {} not confirmed ({}); executing it again",
                                entry_name,
                                reason
                            );
                            futures.push(run_case(entry_name, filter, false));
                        }
                    }
                    continue;
                }
                let result_entries =
                    parsed.buck_error_context("Failed to parse test result output")?;
                let confirmation = confirm_case(&entry_name, exit_code, &result_entries);

                let local_case = match local_case {
                    Some(LocalCaseResult::Reused(reused)) => reused,
                    other => {
                        report_entries(
                            orchestrator,
                            target_handle,
                            &entry_name,
                            exit_code,
                            &result,
                            result_entries,
                            origin.clone(),
                            tracked_fresh,
                        )
                        .await?;
                        settle_fresh(other, confirmation).await;
                        continue;
                    }
                };
                {
                    let reused = local_case;
                    match confirmation {
                        Ok(_) => {}
                        Err(reason) => {
                            // The current parser does not confirm the reused raw streams: hide the
                            // entry and execute the case afresh as a new tracked generation that
                            // does not read the store, reporting only that execution.
                            reject_reused(reused, format!("reused result not confirmed: {reason}"))
                                .await;
                            futures.push(run_case(entry_name, filter, false));
                            continue;
                        }
                    }
                    report_entries(
                        orchestrator,
                        target_handle,
                        &entry_name,
                        exit_code,
                        &result,
                        result_entries,
                        origin.clone(),
                        false,
                    )
                    .await?;
                    // The confirmed reuse is reported; dropping `reused` releases its lease.
                    drop(reused);
                }
            }
            Ok((ExecuteResponse::Cancelled(_), local_case)) => {
                settle_unconfirmed(local_case.take(), "the case was cancelled").await;
                let test_result = TestResult {
                    target: target_handle,
                    name: entry_name,
                    status: TestStatus::OMITTED,
                    msg: Some("Cancelled".to_owned()),
                    duration: None,
                    details: String::new(),
                    max_memory_used_bytes: None,
                    origin: TestResultOrigin::Fresh,
                };
                orchestrator
                    .report_test_result(test_result)
                    .await
                    .buck_error_context("Failed to report test result")?;
            }
            Err(e) => {
                let test_result = TestResult {
                    target: target_handle,
                    name: entry_name,
                    status: TestStatus::FATAL,
                    msg: Some(format!("{:#}", e)),
                    duration: None,
                    details: String::new(),
                    max_memory_used_bytes: None,
                    origin: TestResultOrigin::Fresh,
                };
                orchestrator
                    .report_test_result(test_result)
                    .await
                    .buck_error_context("Failed to report test result")?;
            }
        }
    }

    Ok(())
}

/// Exactly one parsed PASS for the expected case, from an exit-0 execution.
fn confirm_case(
    expected_case: &str,
    exit_code: i32,
    entries: &[TestResultEntry],
) -> Result<ConfirmedPass, crate::local_result_cache::NotConfirmed> {
    let results: Vec<(&str, &TestStatus)> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), &entry.status))
        .collect();
    ConfirmedPass::exact(expected_case, exit_code, &results)
}

/// The entries to report for a tracked fresh case. Its parsed entries are reported unchanged
/// only when they are exactly one result for the expected case that agrees with the exit code, so
/// a genuine parsed FAIL or SKIP stays as reported. An empty, wrong-case or multiple parse, or a
/// PASS from a non-zero exit, is reported as one explicit FAIL for the expected case naming the
/// mismatch, with the raw output: it never reads as an ordinary PASS. Nothing is published for
/// it, since it is not an exact confirmation.
fn strict_fresh_entries(
    expected: &str,
    exit_code: i32,
    execution_time: Duration,
    raw_output: impl FnOnce() -> String,
    entries: Vec<TestResultEntry>,
) -> Vec<TestResultEntry> {
    let mismatch = match entries.as_slice() {
        [only] if only.name == expected => {
            if only.status == TestStatus::PASS && exit_code != 0 {
                Some(format!(
                    "the parser reported PASS for `{expected}`, but the case exited with code \
                     {exit_code}"
                ))
            } else {
                None
            }
        }
        parsed => Some(format!(
            "the parser returned {} result(s) [{}] for `{expected}` (exit code {exit_code}), not \
             exactly one result for that case",
            parsed.len(),
            parsed
                .iter()
                .map(|e| format!("{}: {:?}", e.name, e.status))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    };
    match mismatch {
        None => entries,
        Some(mismatch) => vec![TestResultEntry {
            name: expected.to_owned(),
            status: TestStatus::FAIL,
            message: Some(format!("Unconfirmed case result: {mismatch}")),
            details: Some(raw_output()),
            duration: Some(execution_time),
        }],
    }
}

/// The results reported for a case's parsed entries. A tracked fresh case is reported strictly
/// (`strict_fresh_entries`); any other case keeps the legacy reporting, including its fallback
/// for an empty parse.
fn case_results(
    tracked_fresh: bool,
    target_handle: ConfiguredTargetHandle,
    entry_name: &str,
    exit_code: i32,
    result: &buck2_test_api::data::ExecutionResult2,
    result_entries: Vec<TestResultEntry>,
    origin: TestResultOrigin,
) -> Vec<TestResult> {
    let result_entries = if tracked_fresh {
        strict_fresh_entries(
            entry_name,
            exit_code,
            result.execution_time,
            || format_execution_output(&result.stdout, &result.stderr),
            result_entries,
        )
    } else {
        result_entries
    };
    ordinary_results(
        target_handle,
        entry_name,
        exit_code,
        result,
        result_entries,
        origin,
    )
}

/// The ordinary results for parsed entries, or a fallback derived from the exit code when the
/// parser returned none.
fn ordinary_results(
    target_handle: ConfiguredTargetHandle,
    entry_name: &str,
    exit_code: i32,
    result: &buck2_test_api::data::ExecutionResult2,
    result_entries: Vec<TestResultEntry>,
    // The caller's provenance for these entries: `TestResultOrigin::of_execution` for a
    // confirmed reuse (local or remote), fresh for this run's own execution.
    origin: TestResultOrigin,
) -> Vec<TestResult> {
    if result_entries.is_empty() {
        // Parser returned no results — synthesize a fallback based on
        // exit code to prevent silent test disappearance.
        let status = if exit_code == 0 {
            TestStatus::PASS
        } else {
            TestStatus::FAIL
        };
        vec![TestResult {
            target: target_handle,
            name: entry_name.to_owned(),
            status,
            msg: Some(format!(
                "Parser returned no results (exit code {})",
                exit_code
            )),
            duration: Some(result.execution_time),
            details: format_execution_output(&result.stdout, &result.stderr),
            max_memory_used_bytes: result.max_memory_used_bytes,
            origin,
        }]
    } else {
        result_entries
            .into_iter()
            .map(|res| TestResult {
                target: target_handle,
                name: res.name,
                status: res.status,
                msg: res.message,
                duration: res.duration,
                details: res.details.unwrap_or_default(),
                max_memory_used_bytes: result.max_memory_used_bytes,
                origin: origin.clone(),
            })
            .collect()
    }
}

/// Report a case's parsed entries as `case_results` decides.
async fn report_entries(
    orchestrator: &dyn InternalTestOrchestrator,
    target_handle: ConfiguredTargetHandle,
    entry_name: &str,
    exit_code: i32,
    result: &buck2_test_api::data::ExecutionResult2,
    result_entries: Vec<TestResultEntry>,
    origin: TestResultOrigin,
    tracked_fresh: bool,
) -> buck2_error::Result<()> {
    for test_result in case_results(
        tracked_fresh,
        target_handle,
        entry_name,
        exit_code,
        result,
        result_entries,
        origin,
    ) {
        orchestrator
            .report_test_result(test_result)
            .await
            .buck_error_context("Failed to report test result")?;
    }
    Ok(())
}

/// A deterministic description of a runner spec's command and environment: every command value
/// and every environment entry sorted by name, each length-delimited.
fn spec_identity(spec: &ExternalRunnerSpec) -> String {
    let mut out = String::new();
    let mut push = |value: &str| {
        out.push_str(&value.len().to_string());
        out.push(':');
        out.push_str(value);
    };
    push(&spec.test_type);
    push(&spec.command.len().to_string());
    for value in &spec.command {
        push(&value.to_string());
    }
    let mut env: Vec<(&String, &ExternalRunnerSpecValue)> = spec.env.iter().collect();
    env.sort_by(|a, b| a.0.cmp(b.0));
    push(&env.len().to_string());
    for (name, value) in env {
        push(name);
        push(&value.to_string());
    }
    out
}

fn build_command_from_spec(spec: &ExternalRunnerSpec) -> Vec<ArgValue> {
    spec.command
        .iter()
        .map(|v| ArgValue {
            content: ArgValueContent::ExternalRunnerSpecValue(v.clone()),
            format: None,
        })
        .collect()
}

fn build_env_from_spec(spec: &ExternalRunnerSpec) -> BuckFixedSortedMap<String, ArgValue> {
    spec.env
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                ArgValue {
                    content: ArgValueContent::ExternalRunnerSpecValue(v.clone()),
                    format: None,
                },
            )
        })
        .collect()
}

fn format_execution_output(stdout: &ExecutionStream, stderr: &ExecutionStream) -> String {
    let stdout_str = match stdout {
        ExecutionStream::Inline(bytes) => String::from_utf8_lossy(bytes),
    };
    let stderr_str = match stderr {
        ExecutionStream::Inline(bytes) => String::from_utf8_lossy(bytes),
    };
    format!(
        "---- STDOUT ----\n{}\n---- STDERR ----\n{}\n",
        stdout_str, stderr_str
    )
}

#[cfg(test)]
mod tests {
    use buck2_core::cells::name::CellName;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
    use buck2_test_api::data::ConfiguredTarget;
    use buck2_test_api::data::ExecutionStream;
    use buck2_test_api::data::ExternalRunnerSpec;
    use buck2_test_api::data::ExternalRunnerSpecValue;
    use buck2_test_api::data::testing::ConfiguredTargetHandleExt;

    use super::*;

    fn make_spec(commands: Vec<&str>, env: Vec<(&str, &str)>) -> ExternalRunnerSpec {
        ExternalRunnerSpec {
            target: ConfiguredTarget {
                handle: ConfiguredTargetHandleExt::testing_new(0),
                cell: "root".to_owned(),
                package: "test".to_owned(),
                target: "my_test".to_owned(),
                configuration: "cfg".to_owned(),
                package_project_relative_path: ForwardRelativePathBuf::unchecked_new(
                    "test".to_owned(),
                ),
                test_config_unification_rollout: false,
                package_oncall: None,
            },
            test_type: "rust".to_owned(),
            command: commands
                .into_iter()
                .map(|c| ExternalRunnerSpecValue::Verbatim(c.to_owned()))
                .collect(),
            env: env
                .into_iter()
                .map(|(k, v)| {
                    (
                        k.to_owned(),
                        ExternalRunnerSpecValue::Verbatim(v.to_owned()),
                    )
                })
                .collect(),
            labels: vec![],
            contacts: vec![],
            oncall: None,
            working_dir_cell: CellName::testing_new("root"),
        }
    }

    fn execution(exit_code: i32) -> buck2_test_api::data::ExecutionResult2 {
        buck2_test_api::data::ExecutionResult2 {
            status: ExecutionStatus::Finished {
                exitcode: exit_code,
            },
            stdout: ExecutionStream::Inline(b"raw out \xff".to_vec()),
            stderr: ExecutionStream::Inline(b"raw err".to_vec()),
            outputs: Default::default(),
            start_time: std::time::SystemTime::now(),
            execution_time: Duration::from_millis(9),
            max_memory_used_bytes: None,
            execution_details: Default::default(),
            command_execution: None,
        }
    }

    fn entry(name: &str, status: TestStatus) -> TestResultEntry {
        TestResultEntry {
            name: name.to_owned(),
            status,
            message: None,
            details: None,
            duration: None,
        }
    }

    const CASE: &str = "module::case";

    /// The results the runner reports for a fresh case, as its result loop decides them.
    fn reported(
        tracked_fresh: bool,
        exit_code: i32,
        entries: Vec<TestResultEntry>,
    ) -> Vec<TestResult> {
        case_results(
            tracked_fresh,
            ConfiguredTargetHandleExt::testing_new(0),
            CASE,
            exit_code,
            &execution(exit_code),
            entries,
            TestResultOrigin::Fresh,
        )
    }

    fn statuses(results: &[TestResult]) -> Vec<(&str, TestStatus)> {
        results
            .iter()
            .map(|r| (r.name.as_str(), r.status.clone()))
            .collect()
    }

    #[test]
    fn a_tracked_fresh_case_never_reports_an_unconfirmed_pass() {
        let mismatches = [
            ("empty parse", 0, vec![]),
            (
                "wrong-case PASS",
                0,
                vec![entry("module::other", TestStatus::PASS)],
            ),
            (
                "two PASS entries",
                0,
                vec![entry(CASE, TestStatus::PASS), entry(CASE, TestStatus::PASS)],
            ),
            (
                "expected and another PASS",
                0,
                vec![
                    entry(CASE, TestStatus::PASS),
                    entry("module::other", TestStatus::PASS),
                ],
            ),
            (
                "PASS from a failed exit",
                1,
                vec![entry(CASE, TestStatus::PASS)],
            ),
        ];
        for (name, exit_code, entries) in mismatches {
            // Not an exact confirmation, so the captured generation is rejected, not published.
            assert!(confirm_case(CASE, exit_code, &entries).is_err(), "{name}");
            let results = reported(true, exit_code, entries);
            assert_eq!(statuses(&results), [(CASE, TestStatus::FAIL)], "{name}");
            let message = results[0].msg.as_deref().unwrap();
            assert!(
                message.contains("Unconfirmed case result"),
                "{name}: {message}"
            );
            // The raw output stays with the reported result.
            assert!(results[0].details.contains("raw out"), "{name}");
            assert!(results[0].details.contains("raw err"), "{name}");
        }

        // An exact confirmation, and a genuine parsed FAIL or SKIP, are reported as parsed.
        assert!(confirm_case(CASE, 0, &[entry(CASE, TestStatus::PASS)]).is_ok());
        for (exit_code, status) in [
            (0, TestStatus::PASS),
            (1, TestStatus::FAIL),
            (0, TestStatus::SKIP),
        ] {
            let results = reported(true, exit_code, vec![entry(CASE, status.clone())]);
            assert_eq!(statuses(&results), [(CASE, status)]);
        }
    }

    #[test]
    fn an_untracked_case_keeps_the_legacy_reporting() {
        // An empty parse falls back to the exit code.
        let results = reported(false, 0, vec![]);
        assert_eq!(statuses(&results), [(CASE, TestStatus::PASS)]);
        assert_eq!(
            results[0].msg.as_deref(),
            Some("Parser returned no results (exit code 0)")
        );
        assert_eq!(
            statuses(&reported(false, 1, vec![])),
            [(CASE, TestStatus::FAIL)]
        );
        // Parsed entries are forwarded unchanged.
        let entries = vec![
            entry("module::other", TestStatus::PASS),
            entry(CASE, TestStatus::PASS),
        ];
        assert_eq!(
            statuses(&reported(false, 0, entries)),
            [
                ("module::other", TestStatus::PASS),
                (CASE, TestStatus::PASS)
            ]
        );
    }

    /// A remote command report exactly as upstream's producer writes it
    /// (`CommandExecutionKind::to_proto`) for the given kind of remote answer.
    fn produced(kind: Produced) -> buck2_data::CommandExecutionKind {
        use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
        use buck2_execute::digest_config::DigestConfig;
        use buck2_execute::execute::action_digest::ActionDigest;
        use buck2_execute::execute::kind::CommandExecutionKind;
        use buck2_execute::execute::kind::RemoteCommandExecutionDetails;
        use buck2_execute::execute::kind::RemoteExecutionOrigin;

        let digest = ActionDigest::empty(DigestConfig::testing_default().cas_digest_config());
        let details = RemoteCommandExecutionDetails {
            action_digest: digest.clone(),
            remote_dep_file_key: None,
            session_id: None,
            use_case: RemoteExecutorUseCase::buck2_default(),
            platform: Default::default(),
            persistent_worker: false,
        };
        let remote = |origin| CommandExecutionKind::Remote {
            details: details.clone(),
            queue_time: Duration::ZERO,
            materialized_inputs_for_failed: None,
            materialized_outputs_for_failed_actions: None,
            origin,
            submission: None,
        };
        match kind {
            Produced::ActionCache => CommandExecutionKind::ActionCache {
                details: details.clone(),
            },
            Produced::Executed => remote(RemoteExecutionOrigin::Executed),
            Produced::ServedFromCache => remote(RemoteExecutionOrigin::ServedFromCache),
            Produced::Deduplicated => remote(RemoteExecutionOrigin::Deduplicated),
            Produced::HostStore => CommandExecutionKind::LocalTestResultCache {
                digest,
                cache_key: "key".to_owned(),
                origin_attempt: "origin-attempt".to_owned(),
                entry_manifest: "entry-manifest".to_owned(),
                current_attempt: "current-attempt".to_owned(),
            },
        }
        .to_proto(false)
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Produced {
        ActionCache,
        Executed,
        ServedFromCache,
        Deduplicated,
        HostStore,
    }

    /// An answer as the orchestrator returns it: `execution_time` from the cached ActionResult's
    /// metadata (the original run, 9 s), and the command report's wall time for this run (250 ms,
    /// or unreported).
    fn answer(
        kind: Produced,
        exit_code: i32,
        stdout: &str,
        wall_time: Option<Duration>,
    ) -> buck2_test_api::data::ExecutionResult2 {
        buck2_test_api::data::ExecutionResult2 {
            stdout: ExecutionStream::Inline(stdout.as_bytes().to_vec()),
            stderr: ExecutionStream::Inline(Vec::new()),
            execution_time: Duration::from_secs(9),
            execution_details: buck2_test_api::data::ExecutionDetails {
                execution_kind: Some(produced(kind)),
            },
            command_execution: Some(buck2_data::CommandExecution {
                details: Some(buck2_data::CommandExecutionDetails {
                    metadata: Some(buck2_data::CommandExecutionMetadata {
                        wall_time: wall_time.map(|w| w.try_into().unwrap()),
                        execution_time: Some(Duration::from_secs(9).try_into().unwrap()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..execution(exit_code)
        }
    }

    /// A confirmed answer, with nonzero history, through the real producer
    /// (`CommandExecutionKind::to_proto`), the runner's reporting (`case_results` as the
    /// confirmed-hit branch calls it), the origin, the event translation and the console. The
    /// case's original duration, the original action's execution time and this run's interval
    /// stay distinct and in their own places; `served_by` is present for each remote answer
    /// (`ACTION_CACHE (0)`, `RE_SERVED_FROM_CACHE (3)`, `RE_DEDUPLICATED (4)`) and absent for a
    /// host-store reuse, which is never shown as a remote answer.
    #[test]
    fn reuse_provenance_and_timing_through_producer_translation_and_display()
    -> buck2_error::Result<()> {
        use buck2_core::configuration::data::ConfigurationData;
        use buck2_core::provider::label::ConfiguredProvidersLabel;
        use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
        use buck2_data::RemoteCommandOrigin;
        use buck2_event_observer::display::format_test_result;
        use buck2_event_observer::verbosity::Verbosity;
        use buck2_test_api::data::RemoteReusedTestResult;

        use crate::session::TestSession;
        use crate::session::TestSessionOptions;
        use crate::translations::convert_test_result;

        let session = TestSession::new(TestSessionOptions::default());
        let target = session.register(ConfiguredProvidersLabel::new(
            ConfiguredTargetLabel::testing_parse("cell//pkg:foo", ConfigurationData::testing_new()),
            Default::default(),
        ));
        let digest = match produced(Produced::ActionCache).command {
            Some(buck2_data::command_execution_kind::Command::RemoteCommand(r)) => r.action_digest,
            other => panic!("expected a remote command, got {other:?}"),
        };
        assert!(!digest.is_empty());
        let render = |event: &buck2_data::TestResult| -> buck2_error::Result<String> {
            Ok(format_test_result(event, Verbosity::try_from_cli("3")?)?
                .expect("a reported result is displayed at this verbosity")
                .0[0]
                .to_unstyled())
        };

        for (kind, served_by, origin, source) in [
            (
                Produced::ActionCache,
                RemoteCacheAnswer::ActionCacheLookup,
                RemoteCommandOrigin::ActionCache,
                "reused from remote action cache",
            ),
            (
                Produced::ServedFromCache,
                RemoteCacheAnswer::ServedExecute,
                RemoteCommandOrigin::ReServedFromCache,
                "reused from remote served execute",
            ),
            (
                Produced::Deduplicated,
                RemoteCacheAnswer::Deduplicated,
                RemoteCommandOrigin::ReDeduplicated,
                "reused from remote deduplicated execute",
            ),
        ] {
            for wall_time in [Some(Duration::from_millis(250)), None] {
                let result = answer(kind, 0, &format!("test {CASE} ... ok"), wall_time);
                // The parser confirmed one PASS whose own reported duration is 8 s.
                let parsed = TestResultEntry {
                    duration: Some(Duration::from_secs(8)),
                    ..entry(CASE, TestStatus::PASS)
                };
                assert!(confirm_case(CASE, 0, std::slice::from_ref(&parsed)).is_ok());
                let reported = case_results(
                    false,
                    target,
                    CASE,
                    0,
                    &result,
                    vec![parsed],
                    TestResultOrigin::of_execution(&result),
                );
                let [reported] = <[TestResult; 1]>::try_from(reported).unwrap();
                assert_eq!(reported.duration, Some(Duration::from_secs(8)));
                assert_eq!(
                    reported.origin,
                    TestResultOrigin::RemoteReused(RemoteReusedTestResult {
                        action_digest: digest.clone(),
                        served_by,
                        original_execution_time: Duration::from_secs(9),
                        current_execution_time: wall_time,
                    })
                );

                let event = convert_test_result(reported, &session)?;
                assert_eq!(event.duration, Some(Duration::from_secs(8).try_into()?));
                assert_eq!(
                    event.reuse,
                    Some(Box::new(buck2_data::TestResultReuse {
                        origin_attempt: String::new(),
                        entry_manifest: String::new(),
                        current_attempt: String::new(),
                        current_execution_time: wall_time.map(|w| w.try_into()).transpose()?,
                        action_digest: digest.clone(),
                        served_by: Some(origin as i32),
                        original_execution_time: Some(Duration::from_secs(9).try_into()?),
                    }))
                );
                let current = match wall_time {
                    Some(_) => "0.3s now",
                    None => "current interval unknown",
                };
                assert_eq!(
                    render(&event)?,
                    format!(
                        "✓ Pass: {CASE} ({source} {digest}: 8.0s originally, action ran 9.0s \
                         originally, {current})"
                    )
                );
            }
        }

        // A host-store reuse: `served_by` absent, no remote provenance shown.
        // As the orchestrator returns it: no case ran, so this run's time is zero.
        let result = buck2_test_api::data::ExecutionResult2 {
            execution_time: Duration::ZERO,
            ..answer(Produced::HostStore, 0, &format!("test {CASE} ... ok"), None)
        };
        let reported = case_results(
            false,
            target,
            CASE,
            0,
            &result,
            vec![TestResultEntry {
                duration: Some(Duration::from_secs(8)),
                ..entry(CASE, TestStatus::PASS)
            }],
            TestResultOrigin::of_execution(&result),
        );
        let [reported] = <[TestResult; 1]>::try_from(reported).unwrap();
        let event = convert_test_result(reported, &session)?;
        let reuse = event.reuse.as_deref().expect("a reused result");
        assert_eq!(reuse.served_by, None);
        assert_eq!(reuse.action_digest, "");
        assert_eq!(reuse.original_execution_time, None);
        let line = render(&event)?;
        assert_eq!(
            line,
            format!("✓ Pass: {CASE} (reused: 8.0s originally, 0.0s now)")
        );
        assert!(!line.contains("remote"), "{line}");

        // An executed remote command is fresh: no reuse record at all.
        let result = answer(Produced::Executed, 0, &format!("test {CASE} ... ok"), None);
        assert_eq!(
            TestResultOrigin::of_execution(&result),
            TestResultOrigin::Fresh
        );
        Ok(())
    }

    /// The real runner loop (`run_internal_test`) over a scripted orchestrator, for a provider
    /// whose executors select remote case result reuse.
    mod remote_case_reuse {
        use std::collections::VecDeque;
        use std::sync::Mutex;

        use buck2_build_api::interpreter::rule_defs::command_executor_config::register_command_executor_config;
        use buck2_build_api::interpreter::rule_defs::register_rule_defs;
        use buck2_core::bzl::ImportPath;
        use buck2_interpreter_for_build::interpreter::testing::Tester;
        use buck2_test_api::data::CasDigest;
        use buck2_test_api::data::DeclaredOutput;
        use buck2_test_api::data::ExecutorConfigOverride;
        use buck2_test_api::data::PrepareForLocalExecutionResult;

        use super::*;

        const PROVIDER: &str = r#"
def parse_listing(stdout):
    return [{"name": n, "filter": n} for n in stdout.split("\n") if n]

def parse_result(stdout, stderr, exit_code):
    results = []
    for line in stdout.split("\n"):
        if line == "MALFORMED":
            fail("malformed frame")
        if line.startswith("test ") and " ... " in line:
            name, _sep, word = line[len("test "):].partition(" ... ")
            status = {"ok": "PASS", "FAILED": "FAIL", "ignored": "SKIP"}[word]
            results.append({"name": name, "status": status})
    return results

exported_info = InternalRunnerTestInfo(
    type = "rust",
    command = ["bin"],
    listing_command = ["bin", "--list"],
    parse_test_listing = parse_listing,
    parse_test_result = parse_result,
    supports_test_execution_caching = True,
    default_executor = CommandExecutorConfig(
        local_enabled = False,
        remote_enabled = True,
        remote_cache_enabled = True,
        remote_execution_properties = {"platform": "linux"},
        remote_execution_use_case = "buck2-default",
    ),
)
"#;

        fn provider() -> buck2_error::Result<OwnedInternalRunnerTestInfo> {
            // Each of these tests may run alone, so none relies on another having installed
            // the late-bound globals the provider's evaluation reads.
            crate::testing::init_late_bindings();
            let mut tester = Tester::new()?;
            tester.additional_globals(register_rule_defs);
            tester.additional_globals(register_command_executor_config);
            let loaded = tester.add_import(
                &ImportPath::testing_new("root//test:provider.bzl"),
                PROVIDER,
            )?;
            let info = loaded
                .env()
                .get("exported_info")
                .expect("`exported_info` not found")
                .downcast_starlark::<InternalRunnerTestInfo>()
                .map_err(buck2_error::Error::from)?;
            assert_eq!(
                info.as_ref().value().as_ref().case_result_reuse(),
                CaseResultReuse::RemoteActionCache
            );
            Ok(info)
        }

        /// Answers the listing with one case and each case request with the next scripted
        /// answer, recording every request's `disable_test_execution_caching` and every report.
        struct Scripted {
            answers: Mutex<VecDeque<buck2_test_api::data::ExecutionResult2>>,
            requests: Mutex<Vec<bool>>,
            reports: Mutex<Vec<TestResult>>,
        }

        #[async_trait]
        impl TestOrchestrator for Scripted {
            async fn execute2(
                &self,
                stage: TestStage,
                _target: ConfiguredTargetHandle,
                _cmd: Vec<ArgValue>,
                _env: BuckFixedSortedMap<String, ArgValue>,
                _timeout: Duration,
                _host_sharing_requirements: HostSharingRequirements,
                _pre_create_dirs: Vec<DeclaredOutput>,
                _executor_override: Option<ExecutorConfigOverride>,
                _required_local_resources: RequiredLocalResources,
                _disable_test_execution_caching: bool,
            ) -> buck2_error::Result<ExecuteResponse> {
                assert!(matches!(stage, TestStage::Listing { .. }), "{stage:?}");
                Ok(ExecuteResponse::Result(
                    buck2_test_api::data::ExecutionResult2 {
                        stdout: ExecutionStream::Inline(format!("{CASE}\n").into_bytes()),
                        ..execution(0)
                    },
                ))
            }
            async fn report_test_result(&self, r: TestResult) -> buck2_error::Result<()> {
                self.reports.lock().unwrap().push(r);
                Ok(())
            }
            async fn report_tests_discovered(
                &self,
                _target: ConfiguredTargetHandle,
                _suite: String,
                _names: Vec<String>,
            ) -> buck2_error::Result<()> {
                Ok(())
            }
            async fn report_test_session(
                &self,
                _session_info: String,
                _test_session_id: Option<String>,
            ) -> buck2_error::Result<()> {
                unimplemented!()
            }
            async fn end_of_test_results(&self, _exit_code: i32) -> buck2_error::Result<()> {
                unimplemented!()
            }
            async fn prepare_for_local_execution(
                &self,
                _stage: TestStage,
                _target: ConfiguredTargetHandle,
                _cmd: Vec<ArgValue>,
                _env: BuckFixedSortedMap<String, ArgValue>,
                _pre_create_dirs: Vec<DeclaredOutput>,
                _required_local_resources: RequiredLocalResources,
            ) -> buck2_error::Result<PrepareForLocalExecutionResult> {
                unimplemented!()
            }
            async fn attach_info_message(&self, _message: String) -> buck2_error::Result<()> {
                unimplemented!()
            }
            async fn upload_to_cas(
                &self,
                _local_path: String,
                _ttl_seconds: i64,
                _use_case: String,
            ) -> buck2_error::Result<CasDigest> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl InternalTestOrchestrator for Scripted {
            async fn execute_case(
                &self,
                stage: TestStage,
                _test_target: ConfiguredTargetHandle,
                _cmd: Vec<ArgValue>,
                _env: BuckFixedSortedMap<String, ArgValue>,
                _timeout: Duration,
                _required_local_resources: RequiredLocalResources,
                disable_test_execution_caching: bool,
                local_case: Option<LocalCaseRequest>,
            ) -> buck2_error::Result<(ExecuteResponse, LocalCaseOutcome)> {
                assert!(matches!(stage, TestStage::Testing { .. }), "{stage:?}");
                // Remote reuse never asks the host-local case store.
                assert!(local_case.is_none());
                self.requests
                    .lock()
                    .unwrap()
                    .push(disable_test_execution_caching);
                let answer = self
                    .answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("an unscripted case request");
                Ok((ExecuteResponse::Result(answer), LocalCaseOutcome::default()))
            }
        }

        /// Runs one case against the scripted answers; returns each request's
        /// `disable_test_execution_caching` and the reported results.
        async fn run(
            answers: Vec<buck2_test_api::data::ExecutionResult2>,
        ) -> buck2_error::Result<(Vec<bool>, Vec<TestResult>)> {
            let scripted = Scripted {
                answers: Mutex::new(answers.into()),
                requests: Mutex::new(Vec::new()),
                reports: Mutex::new(Vec::new()),
            };
            let spec = make_spec(vec!["bin"], vec![]);
            let listing_spec = make_spec(vec!["bin", "--list"], vec![]);
            run_internal_test(
                &scripted,
                spec,
                listing_spec,
                &provider()?,
                Duration::from_secs(60),
            )
            .await?;
            assert!(
                scripted.answers.lock().unwrap().is_empty(),
                "every scripted answer is used"
            );
            Ok((
                scripted.requests.into_inner().unwrap(),
                scripted.reports.into_inner().unwrap(),
            ))
        }

        fn ok() -> String {
            format!("test {CASE} ... ok")
        }

        fn only(reports: &[TestResult]) -> &TestResult {
            assert_eq!(reports.len(), 1, "{reports:?}");
            &reports[0]
        }

        fn served_by(result: &TestResult) -> Option<RemoteCacheAnswer> {
            match &result.origin {
                TestResultOrigin::RemoteReused(r) => Some(r.served_by),
                _ => None,
            }
        }

        /// Each confirmed cached answer is reported once as remote reuse, with no second
        /// request.
        #[tokio::test]
        async fn a_confirmed_cached_answer_is_reported_as_remote_reuse() -> buck2_error::Result<()>
        {
            for (kind, expected) in [
                (Produced::ActionCache, RemoteCacheAnswer::ActionCacheLookup),
                (Produced::ServedFromCache, RemoteCacheAnswer::ServedExecute),
                (Produced::Deduplicated, RemoteCacheAnswer::Deduplicated),
            ] {
                let wall = Some(Duration::from_millis(250));
                let (requests, reports) = run(vec![answer(kind, 0, &ok(), wall)]).await?;
                assert_eq!(requests, [false], "{kind:?}");
                let result = only(&reports);
                assert_eq!(result.status, TestStatus::PASS, "{kind:?}");
                assert_eq!(served_by(result), Some(expected), "{kind:?}");
            }
            Ok(())
        }

        /// A cache miss runs the case: reported strictly and fresh.
        #[tokio::test]
        async fn an_executed_case_is_fresh_and_strict() -> buck2_error::Result<()> {
            let (requests, reports) = run(vec![answer(Produced::Executed, 0, &ok(), None)]).await?;
            assert_eq!(requests, [false]);
            let result = only(&reports);
            assert_eq!(result.status, TestStatus::PASS);
            assert_eq!(result.origin, TestResultOrigin::Fresh);
            // An empty parse of an execution is never a legacy PASS here.
            let (_, reports) = run(vec![answer(Produced::Executed, 0, "", None)]).await?;
            let result = only(&reports);
            assert_eq!(result.status, TestStatus::FAIL);
            assert!(
                result
                    .msg
                    .as_deref()
                    .unwrap()
                    .contains("Unconfirmed case result")
            );
            Ok(())
        }

        /// Anything the current parser does not confirm as one PASS for the case (FAIL, SKIP,
        /// an empty or malformed parse, a nonzero exit, a timeout) is never reported: that case
        /// alone executes again with caching disabled, and only that execution is reported.
        #[tokio::test]
        async fn an_unconfirmed_cached_answer_reruns_the_case_bypassing_the_cache()
        -> buck2_error::Result<()> {
            let timed_out = |kind| buck2_test_api::data::ExecutionResult2 {
                status: ExecutionStatus::TimedOut {
                    duration: Duration::from_secs(60),
                },
                ..answer(kind, 0, "", None)
            };
            for kind in [
                Produced::ActionCache,
                Produced::ServedFromCache,
                Produced::Deduplicated,
            ] {
                for unconfirmed in [
                    answer(kind, 0, &format!("test {CASE} ... FAILED"), None),
                    answer(kind, 0, &format!("test {CASE} ... ignored"), None),
                    answer(kind, 0, "", None),
                    answer(kind, 0, "MALFORMED", None),
                    answer(kind, 0, "test module::other ... ok", None),
                    answer(kind, 1, &ok(), None),
                    timed_out(kind),
                ] {
                    let (requests, reports) = run(vec![
                        unconfirmed,
                        answer(
                            Produced::Executed,
                            1,
                            &format!("test {CASE} ... FAILED"),
                            None,
                        ),
                    ])
                    .await?;
                    assert_eq!(requests, [false, true], "{kind:?}");
                    let result = only(&reports);
                    assert_eq!(result.status, TestStatus::FAIL, "{kind:?}");
                    assert_eq!(result.origin, TestResultOrigin::Fresh, "{kind:?}");
                }
            }
            Ok(())
        }

        /// An action-cache or served-from-cache answer to the cache-bypassing rerun is FATAL,
        /// whatever it says.
        #[tokio::test]
        async fn a_cached_answer_to_the_bypass_rerun_is_fatal() -> buck2_error::Result<()> {
            for bypass in [Produced::ActionCache, Produced::ServedFromCache] {
                let (requests, reports) = run(vec![
                    answer(Produced::ActionCache, 0, "", None),
                    answer(bypass, 0, &ok(), None),
                ])
                .await?;
                assert_eq!(requests, [false, true], "{bypass:?}");
                let result = only(&reports);
                assert_eq!(result.status, TestStatus::FATAL, "{bypass:?}");
                assert!(
                    result
                        .msg
                        .as_deref()
                        .unwrap()
                        .contains("cache-bypassing execution"),
                    "{bypass:?}"
                );
            }
            Ok(())
        }

        /// A deduplicated answer to the bypass rerun came from a real execution of the
        /// identical action already in flight: reported strictly as this run's execution, never
        /// FATAL and never a reused PASS.
        #[tokio::test]
        async fn a_deduplicated_bypass_rerun_is_reported_strictly() -> buck2_error::Result<()> {
            let (requests, reports) = run(vec![
                answer(Produced::ActionCache, 0, "", None),
                answer(Produced::Deduplicated, 0, &ok(), None),
            ])
            .await?;
            assert_eq!(requests, [false, true]);
            let result = only(&reports);
            assert_eq!(result.status, TestStatus::PASS);
            assert_eq!(result.origin, TestResultOrigin::Fresh);

            // Strict: an empty parse is an explicit FAIL, not the legacy exit-code PASS.
            let (requests, reports) = run(vec![
                answer(Produced::ServedFromCache, 0, "", None),
                answer(Produced::Deduplicated, 0, "", None),
            ])
            .await?;
            assert_eq!(requests, [false, true]);
            let result = only(&reports);
            assert_eq!(result.status, TestStatus::FAIL);
            assert_eq!(result.origin, TestResultOrigin::Fresh);
            assert!(
                result
                    .msg
                    .as_deref()
                    .unwrap()
                    .contains("Unconfirmed case result")
            );

            // A genuine FAIL stays a FAIL.
            let (_, reports) = run(vec![
                answer(Produced::Deduplicated, 0, "", None),
                answer(
                    Produced::Deduplicated,
                    1,
                    &format!("test {CASE} ... FAILED"),
                    None,
                ),
            ])
            .await?;
            let result = only(&reports);
            assert_eq!(result.status, TestStatus::FAIL);
            assert_eq!(result.origin, TestResultOrigin::Fresh);
            Ok(())
        }
    }

    #[test]
    fn test_build_command_from_spec() {
        let spec = make_spec(vec!["binary", "--flag"], vec![]);
        let cmd = build_command_from_spec(&spec);
        assert_eq!(cmd.len(), 2);
        assert_eq!(
            cmd[0].content,
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::Verbatim(
                "binary".to_owned()
            ))
        );
        assert_eq!(
            cmd[1].content,
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::Verbatim(
                "--flag".to_owned()
            ))
        );
    }

    #[test]
    fn test_build_env_from_spec() {
        let spec = make_spec(vec![], vec![("FOO", "bar"), ("BAZ", "qux")]);
        let env = build_env_from_spec(&spec);
        assert_eq!(env.len(), 2);
        assert!(env.contains_key("BAZ"));
        assert!(env.contains_key("FOO"));
    }

    #[test]
    fn test_format_execution_output() {
        let stdout = ExecutionStream::Inline(b"hello".to_vec());
        let stderr = ExecutionStream::Inline(b"world".to_vec());
        let output = format_execution_output(&stdout, &stderr);
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("STDOUT"));
        assert!(output.contains("STDERR"));
    }

    #[test]
    fn test_format_execution_output_empty() {
        let stdout = ExecutionStream::Inline(vec![]);
        let stderr = ExecutionStream::Inline(vec![]);
        let output = format_execution_output(&stdout, &stderr);
        assert_eq!(output, "---- STDOUT ----\n\n---- STDERR ----\n\n");
    }
}
