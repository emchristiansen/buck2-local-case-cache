/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

pub enum LastCommandExecutionKind {
    Local,
    LocalWorker,
    Remote,
    /// Submitted for remote execution, but the service served its own action cache instead.
    RemoteServedFromCache,
    /// Executed remotely as an execution shared with another build: the service deduplicated it
    /// against an identical execution already in flight.
    RemoteDeduplicated,
    Cached,
    RemoteDepFileCached,
    LocalTestResultCached,
    NoCommand,
}

/// Returns what the execution kind of the last command was in the given action.
/// It tells the execution kind of the commands that actually produced a result
/// or an error, but not the commands that fell back and were retried.
pub fn get_last_command_execution_kind(
    action: &buck2_data::ActionExecutionEnd,
) -> LastCommandExecutionKind {
    let last_command_kind = action
        .commands
        .last()
        .and_then(|c| c.details.as_ref())
        .and_then(|c| c.command_kind.as_ref());

    if let Some(command_kind) = last_command_kind {
        use buck2_data::command_execution_kind::Command;
        match command_kind.command.as_ref() {
            Some(Command::LocalCommand(..)) | Some(Command::OmittedLocalCommand(..)) => {
                LastCommandExecutionKind::Local
            }
            Some(Command::WorkerCommand(_)) | Some(Command::WorkerInitCommand(_)) => {
                LastCommandExecutionKind::LocalWorker
            }
            Some(Command::RemoteCommand(remote)) => match remote.cache_hit_type() {
                buck2_data::RemoteCommandOrigin::ActionCache => LastCommandExecutionKind::Cached,
                buck2_data::RemoteCommandOrigin::RemoteDepFileCache => {
                    LastCommandExecutionKind::RemoteDepFileCached
                }
                buck2_data::RemoteCommandOrigin::Executed => LastCommandExecutionKind::Remote,
                buck2_data::RemoteCommandOrigin::ReServedFromCache => {
                    LastCommandExecutionKind::RemoteServedFromCache
                }
                buck2_data::RemoteCommandOrigin::ReDeduplicated => {
                    LastCommandExecutionKind::RemoteDeduplicated
                }
            },
            Some(Command::LocalTestResultCache(_)) => {
                LastCommandExecutionKind::LocalTestResultCached
            }
            None => LastCommandExecutionKind::NoCommand,
        }
    } else {
        LastCommandExecutionKind::NoCommand
    }
}

/// Returns whether a test run's result was reused from the local test-result cache rather than
/// produced by running the test. The `TestRunEnd` command report is the single authoritative
/// record of such a hit; the lookup/hit executor stages that precede it are not counted.
pub fn is_local_test_result_reuse(test_run: &buck2_data::TestRunEnd) -> bool {
    matches!(
        test_run
            .command_report
            .as_ref()
            .and_then(|c| c.details.as_ref())
            .and_then(|d| d.command_kind.as_ref())
            .and_then(|k| k.command.as_ref()),
        Some(buck2_data::command_execution_kind::Command::LocalTestResultCache(_))
    )
}

pub struct ExecTime {
    pub exec_time_ms: u64,
    pub(crate) cached_exec_time_ms: u64,
}

pub fn get_last_command_execution_time(action: &buck2_data::ActionExecutionEnd) -> ExecTime {
    if matches!(
        get_last_command_execution_kind(action),
        LastCommandExecutionKind::LocalTestResultCached
    ) {
        // An entry's original runtime is provenance, not a new child interval.
        return ExecTime {
            exec_time_ms: 0,
            cached_exec_time_ms: 0,
        };
    }
    let exec_time_ms = action
        .commands
        .last()
        .and_then(|c| c.details.as_ref())
        .and_then(|c| c.metadata.as_ref())
        .and_then(|c| c.execution_time.as_ref())
        .map_or(0, |c| c.seconds as u64 * 1000 + c.nanos as u64 / 1000000);

    ExecTime {
        exec_time_ms,
        cached_exec_time_ms: match get_last_command_execution_kind(action) {
            LastCommandExecutionKind::Cached | LastCommandExecutionKind::RemoteDepFileCached => {
                exec_time_ms
            }
            _ => 0,
        },
    }
}

#[cfg(test)]
mod local_test_result_cache_tests {
    use super::*;

    #[test]
    fn local_reuse_is_distinct_and_never_a_new_execution_interval() {
        let action = buck2_data::ActionExecutionEnd {
            commands: vec![buck2_data::CommandExecution {
                details: Some(buck2_data::CommandExecutionDetails {
                    command_kind: Some(buck2_data::CommandExecutionKind {
                        command: Some(
                            buck2_data::command_execution_kind::Command::LocalTestResultCache(
                                buck2_data::LocalTestResultCache {
                                    action_digest: "digest".to_owned(),
                                    cache_key: "full-key-digest".to_owned(),
                                    origin_attempt: "original".to_owned(),
                                    entry_manifest: "immutable-manifest".to_owned(),
                                    current_attempt: "current".to_owned(),
                                },
                            ),
                        ),
                    }),
                    metadata: Some(buck2_data::CommandExecutionMetadata {
                        execution_time: Some(prost_types::Duration {
                            seconds: 9,
                            nanos: 0,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(matches!(
            get_last_command_execution_kind(&action),
            LastCommandExecutionKind::LocalTestResultCached
        ));
        let time = get_last_command_execution_time(&action);
        assert_eq!(time.exec_time_ms, 0);
        assert_eq!(time.cached_exec_time_ms, 0);
        assert_eq!(
            buck2_data::get_action_digest(&action.commands).as_deref(),
            Some("digest")
        );
    }
}
