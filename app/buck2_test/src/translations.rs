/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Translation between buck core data and the test spec data types

use buck2_common::file_ops::metadata::FileDigest;
use buck2_core::cells::CellResolver;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_data::ToProtoMessage;
use buck2_error::BuckErrorContext;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionSharedDirectory;
use buck2_test_api::data::ConfiguredTarget;
use buck2_test_api::data::RemoteObject;

use crate::session::TestSession;

pub(crate) fn build_configured_target_handle(
    target: ConfiguredProvidersLabel,
    session: &TestSession,
    cell_resolver: &CellResolver,
    test_config_unification_rollout: bool,
    package_oncall: Option<String>,
) -> buck2_error::Result<ConfiguredTarget> {
    let label = target.target().unconfigured();
    let cell = label.pkg().cell_name().to_string();
    let package = label.pkg().cell_relative_path().to_string();
    let target_name = if cfg!(fbcode_build) {
        label.name().to_string()
    } else {
        label.name().to_string() + &target.name().to_string()
    };
    let configuration = target.cfg().to_string();
    let package_project_relative_path = cell_resolver
        .resolve_path(label.pkg().as_cell_path())
        .buck_error_context("Failed to resolve the project relative path of package")?;

    Ok(ConfiguredTarget {
        handle: session.register(target),
        cell,
        package,
        target: target_name,
        configuration,
        package_project_relative_path: package_project_relative_path.into(),
        test_config_unification_rollout,
        package_oncall,
    })
}

pub(crate) fn convert_test_result(
    test_result: buck2_test_api::data::TestResult,
    session: &TestSession,
) -> buck2_error::Result<buck2_data::TestResult> {
    let buck2_test_api::data::TestResult {
        name,
        status,
        msg,
        duration,
        details,
        target: test_target,
        max_memory_used_bytes,
        origin,
    } = test_result;

    let test_target = session.get(test_target)?;

    Ok(buck2_data::TestResult {
        name,
        status: status.try_into().buck_error_context("Invalid `status`")?,
        msg: msg.map(|msg| buck2_data::test_result::OptionalMsg { msg }),
        duration: duration.and_then(|d| d.try_into().ok()),
        details,
        target_label: Some(test_target.target().as_proto()),
        max_memory_used_bytes,
        reuse: convert_test_result_origin(origin),
    })
}

fn convert_test_result_origin(
    origin: buck2_test_api::data::TestResultOrigin,
) -> Option<Box<buck2_data::TestResultReuse>> {
    match origin {
        buck2_test_api::data::TestResultOrigin::Fresh => None,
        buck2_test_api::data::TestResultOrigin::Reused(
            buck2_test_api::data::ReusedTestResult {
                origin_attempt,
                entry_manifest,
                current_attempt,
                current_execution_time,
            },
        ) => Some(Box::new(buck2_data::TestResultReuse {
            origin_attempt,
            entry_manifest,
            current_attempt,
            current_execution_time: current_execution_time.try_into().ok(),
            // A host-local entry has no remote provenance: `served_by` stays absent.
            action_digest: String::new(),
            served_by: None,
            original_execution_time: None,
        })),
        // The original execution's time goes to `original_execution_time`; only this run's own
        // interval is `current_execution_time`, and an unreported one stays unset (unknown).
        buck2_test_api::data::TestResultOrigin::RemoteReused(
            buck2_test_api::data::RemoteReusedTestResult {
                action_digest,
                served_by,
                original_execution_time,
                current_execution_time,
            },
        ) => Some(Box::new(buck2_data::TestResultReuse {
            origin_attempt: String::new(),
            entry_manifest: String::new(),
            current_attempt: String::new(),
            current_execution_time: current_execution_time.and_then(|d| d.try_into().ok()),
            action_digest,
            served_by: Some(served_by.to_proto() as i32),
            original_execution_time: original_execution_time.try_into().ok(),
        })),
    }
}

/// Convert a named [`ArtifactValue`] into a test API's [`RemoteObject`].
///
/// Note that artifact trees containing symlinks currently can't be converted.
/// Test outputs are unlikely to contain symlinks, and if they do, we'd rather
/// fall back to materializing them on disk.
pub(crate) fn convert_artifact(name: String, artifact: &ArtifactValue) -> Option<RemoteObject> {
    // deps represent artifacts that symlinks depend on. Bail when present.
    if artifact.deps().is_some() {
        return None;
    }

    convert_directory_entry(name, artifact.entry())
}

fn convert_digest(digest: &FileDigest) -> buck2_test_api::data::CasDigest {
    let hash = format!("{}", digest.raw_digest());
    buck2_test_api::data::CasDigest {
        hash,
        size_bytes: digest.size() as i64,
    }
}

fn convert_directory_entry(
    name: String,
    entry: &ActionDirectoryEntry<ActionSharedDirectory>,
) -> Option<RemoteObject> {
    match entry {
        ActionDirectoryEntry::Leaf(
            ActionDirectoryMember::Symlink(..) | ActionDirectoryMember::ExternalSymlink(..),
        ) => None,
        ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => {
            Some(RemoteObject::file(name, convert_digest(f.digest.data())))
        }
        ActionDirectoryEntry::Dir(dir) => {
            let children = dir
                .entries()
                .into_iter()
                .map(|(name, entry)| convert_directory_entry(name.to_string(), entry))
                .collect::<Option<Vec<_>>>()?;
            let digest = convert_digest(dir.fingerprint().data());
            Some(RemoteObject::dir(name, digest, children))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::SystemTime;

    use buck2_core::configuration::data::ConfigurationData;
    use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
    use buck2_event_observer::display::format_test_result;
    use buck2_event_observer::verbosity::Verbosity;
    use buck2_test_api::data::ExecutionDetails;
    use buck2_test_api::data::ExecutionResult2;
    use buck2_test_api::data::ExecutionStatus;
    use buck2_test_api::data::ExecutionStream;
    use buck2_test_api::data::TestResult;
    use buck2_test_api::data::TestResultOrigin;
    use buck2_test_api::data::TestStatus;

    use super::*;
    use crate::session::TestSessionOptions;

    /// The execution result a case run returns: a fresh local execution, or a local test-result
    /// cache hit as the orchestrator reports one (no child ran, zero execution time).
    fn execution(reused: bool) -> ExecutionResult2 {
        use buck2_data::command_execution_kind::Command;
        let command = if reused {
            Command::LocalTestResultCache(buck2_data::LocalTestResultCache {
                action_digest: "digest".to_owned(),
                cache_key: "key".to_owned(),
                origin_attempt: "origin-attempt".to_owned(),
                entry_manifest: "entry-manifest".to_owned(),
                current_attempt: "current-attempt".to_owned(),
            })
        } else {
            Command::LocalCommand(Default::default())
        };
        ExecutionResult2 {
            status: ExecutionStatus::Finished { exitcode: 0 },
            stdout: ExecutionStream::Inline(b"test case ... ok".to_vec()),
            stderr: ExecutionStream::Inline(Vec::new()),
            outputs: Default::default(),
            start_time: SystemTime::now(),
            execution_time: if reused {
                Duration::ZERO
            } else {
                Duration::from_secs(9)
            },
            max_memory_used_bytes: None,
            execution_details: ExecutionDetails {
                execution_kind: Some(buck2_data::CommandExecutionKind {
                    command: Some(command),
                }),
            },
            command_execution: None,
        }
    }

    /// A fresh and a reused result whose parsed entries are identical (same 9 s duration and
    /// details) stay distinguishable through the report translation and the console formatter.
    #[test]
    fn reused_and_fresh_results_are_distinguishable_when_reported() -> buck2_error::Result<()> {
        let session = TestSession::new(TestSessionOptions::default());
        let target = session.register(ConfiguredProvidersLabel::new(
            ConfiguredTargetLabel::testing_parse("cell//pkg:foo", ConfigurationData::testing_new()),
            Default::default(),
        ));
        let report = |reused: bool| -> buck2_error::Result<buck2_data::TestResult> {
            // As the internal runner reports a parsed entry from this execution's output.
            convert_test_result(
                TestResult {
                    target,
                    name: "case".to_owned(),
                    status: TestStatus::PASS,
                    msg: None,
                    duration: Some(Duration::from_secs(9)),
                    details: "test case ... ok".to_owned(),
                    max_memory_used_bytes: None,
                    origin: TestResultOrigin::of_execution(&execution(reused)),
                },
                &session,
            )
        };
        let fresh = report(false)?;
        let reused = report(true)?;

        assert_eq!(fresh.reuse, None);
        assert_eq!(
            reused.reuse,
            Some(Box::new(buck2_data::TestResultReuse {
                origin_attempt: "origin-attempt".to_owned(),
                entry_manifest: "entry-manifest".to_owned(),
                current_attempt: "current-attempt".to_owned(),
                current_execution_time: Some(Duration::ZERO.try_into()?),
                action_digest: String::new(),
                served_by: None,
                original_execution_time: None,
            }))
        );
        // Everything but the provenance is the parsed entry, unchanged.
        assert_eq!(
            buck2_data::TestResult {
                reuse: None,
                ..reused.clone()
            },
            fresh
        );

        let render = |result: &buck2_data::TestResult| -> buck2_error::Result<Vec<String>> {
            Ok(format_test_result(result, Verbosity::try_from_cli("1")?)?
                .expect("a result with details is displayed")
                .0
                .iter()
                .map(|line| line.to_unstyled())
                .collect())
        };
        assert_eq!(render(&fresh)?, ["✓ Pass: case (9.0s)", "test case ... ok"]);
        assert_eq!(
            render(&reused)?,
            [
                "✓ Pass: case (reused: 9.0s originally, 0.0s now)",
                "test case ... ok"
            ]
        );
        Ok(())
    }
}
