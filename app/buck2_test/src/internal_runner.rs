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
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::InternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::OwnedInternalRunnerTestInfo;
use buck2_error::BuckErrorContext;
use buck2_test_api::data::ArgValue;
use buck2_test_api::data::ArgValueContent;
use buck2_test_api::data::ConfiguredTargetHandle;
use buck2_test_api::data::ExecuteResponse;
use buck2_test_api::data::ExecutionStatus;
use buck2_test_api::data::ExecutionStream;
use buck2_test_api::data::ExternalRunnerSpec;
use buck2_test_api::data::ExternalRunnerSpecValue;
use buck2_test_api::data::LocalResourceType;
use buck2_test_api::data::RequiredLocalResources;
use buck2_test_api::data::TestResult;
use buck2_test_api::data::TestResultEntry;
use buck2_test_api::data::TestStage;
use buck2_test_api::data::TestStatus;
use buck2_test_api::protocol::TestOrchestrator;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use host_sharing::HostSharingRequirements;
use sorted_vector_map::SortedVectorMap;

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
        env: SortedVectorMap<String, ArgValue>,
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
    let (required_local_resources, requests_local_case_reuse, test_type, parsers) = {
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
            provider.supports_test_execution_caching(),
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
                    false,
                    local_case,
                )
                .await;
            (entry_name, filter, test_response)
        }
    };

    let mut futures = FuturesUnordered::new();
    for entry in &discovered_tests {
        futures.push(run_case(entry.name.clone(), entry.filter.clone(), true));
    }

    while let Some((entry_name, filter, test_response)) = futures.next().await {
        match test_response {
            Ok((ExecuteResponse::Result(result), local_case)) => {
                let local_case = local_case.take();
                // A fresh generation recorded for an eligible case, successful or not.
                let tracked_fresh = matches!(
                    local_case,
                    Some(LocalCaseResult::Captured(_) | LocalCaseResult::Unsuccessful)
                );
                let exit_code = match result.status {
                    ExecutionStatus::Finished { exitcode } => exitcode,
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

                let result_entries = InternalRunnerTestInfo::parse_test_result_output(
                    provider,
                    &stdout_str,
                    &stderr_str,
                    exit_code,
                )
                .buck_error_context("Failed to parse test result output")?;
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
    ordinary_results(target_handle, entry_name, exit_code, result, result_entries)
}

/// The ordinary results for parsed entries, or a fallback derived from the exit code when the
/// parser returned none.
fn ordinary_results(
    target_handle: ConfiguredTargetHandle,
    entry_name: &str,
    exit_code: i32,
    result: &buck2_test_api::data::ExecutionResult2,
    result_entries: Vec<TestResultEntry>,
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
    tracked_fresh: bool,
) -> buck2_error::Result<()> {
    for test_result in case_results(
        tracked_fresh,
        target_handle,
        entry_name,
        exit_code,
        result,
        result_entries,
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

fn build_env_from_spec(
    spec: &ExternalRunnerSpec,
) -> sorted_vector_map::SortedVectorMap<String, ArgValue> {
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
