/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// prost represents proto `map` fields as `std::collections::HashMap`.
#![allow(clippy::disallowed_types)]

use std::borrow::Cow;
use std::fmt;

use buck2_miniperf_proto::MiniperfCounter;

pub mod action_key_owner;
pub mod agent_context_keys;

mod serialize_bytes {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let d = hex::encode(value);
        d.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let d = String::deserialize(deserializer)?;
        let d = hex::decode(d).map_err(serde::de::Error::custom)?;
        Ok(d)
    }
}

mod serialize_action_kind {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(value: &i32, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let k = crate::ActionKind::try_from(*value).map_err(|_| {
            serde::ser::Error::custom(format!("Invalid ActionKind enum value: {value}"))
        })?;
        k.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<i32, D::Error>
    where
        D: Deserializer<'de>,
    {
        let d = crate::ActionKind::deserialize(deserializer)?;
        Ok(d as i32)
    }
}

tonic::include_proto!("buck.data");

impl CallerKind {
    /// Stable lowercase label for caller telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Agent => "agent",
            Self::CommandShell => "command_shell",
            Self::Other => "other",
            Self::Wrapper => "wrapper",
        }
    }
}

impl AgentDetectionStopReason {
    /// Stable lowercase label for caller telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Agent => "agent",
            Self::Other => "other",
            Self::Unknown => "unknown",
            Self::NoParent => "no_parent",
            Self::ClientUnavailable => "client_unavailable",
            Self::ParentUnavailable => "parent_unavailable",
            Self::NewerParent => "newer_parent",
            Self::Cycle => "cycle",
            Self::DepthLimit => "depth_limit",
        }
    }
}

// Millions of these can be queued. Keep rare large payloads boxed so they
// do not increase the allocation size of every common event.
mini_vec::size_assert::words_of_expr!(BuckEvent::default(), ~40);

pub mod error {
    tonic::include_proto!("buck.data.error");
}

// Preserve the event producers' `Into<Data>` conversions when a large variant is
// boxed. The generated derives also provide conversions from the boxed payloads.
macro_rules! impl_from_boxed_event {
    ($data:ident, $($variant:ident($event:ident)),+ $(,)?) => {
        $(
            impl From<$event> for $data::Data {
                fn from(event: $event) -> Self {
                    Self::$variant(Box::new(event))
                }
            }
        )+
    };
}

impl_from_boxed_event!(
    span_end_event,
    DepFileUpload(DepFileUploadEnd),
    TestDiscovery(TestDiscoveryEnd),
    TestRun(TestRunEnd),
);
impl_from_boxed_event!(
    instant_event,
    ActionError(ActionError),
    CleanStaleResult(CleanStaleResult),
    PageOutSummary(PageOutSummary),
    PagingSummary(PagingSummary),
    RageResult(RageResult),
    ResourceControlEvent(ResourceControlEvent),
);

/// Extract action digest from a list of command executions.
/// Returns the action digest from the last command execution if available.
pub fn get_action_digest(commands: &[CommandExecution]) -> Option<String> {
    if let Some(command_execution) = commands.last() {
        if let Some(details) = &command_execution.details {
            if let Some(command_kind) = &details.command_kind {
                if let Some(command) = &command_kind.command {
                    return match command {
                        command_execution_kind::Command::RemoteCommand(remote_command) => {
                            Some(remote_command.action_digest.to_owned())
                        }
                        command_execution_kind::Command::LocalCommand(local_command) => {
                            Some(local_command.action_digest.to_owned())
                        }
                        command_execution_kind::Command::WorkerCommand(worker_command) => {
                            Some(worker_command.action_digest.to_owned())
                        }
                        command_execution_kind::Command::OmittedLocalCommand(
                            omitted_local_command,
                        ) => Some(omitted_local_command.action_digest.to_owned()),
                        command_execution_kind::Command::LocalTestResultCache(reused) => {
                            Some(reused.action_digest.to_owned())
                        }
                        _ => None,
                    };
                }
            }
        }
    }
    None
}

/// Trait for things that can be converted into protobuf messages, for ease of emitting events. There are many core Buck
/// types that are represented in the Daemon API that use this trait to ease conversion.
pub trait ToProtoMessage {
    type Message: prost::Message;

    fn as_proto(&self) -> Self::Message;
}

// Lives here rather than on `MiniperfCounter` because the orphan rule allows
// only these two crates, and `buck2_miniperf_proto` deliberately has no
// `buck2_data` dep; see `buck2_miniperf_proto/BUCK`.
impl From<MiniperfCounter> for CpuCounter {
    fn from(counter: MiniperfCounter) -> Self {
        CpuCounter {
            count: counter.count,
            time_enabled: counter.time_enabled,
            time_running: counter.time_running,
        }
    }
}

impl fmt::Display for DaemonShutdown {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}, caller:", self.reason)?;

        for caller in self.callers.iter() {
            let max_len = 200;

            let short_caller = if caller.len() > max_len {
                Cow::Owned(
                    caller
                        .chars()
                        .take(max_len)
                        .chain(std::iter::repeat_n('.', 3))
                        .collect(),
                )
            } else {
                Cow::Borrowed(caller)
            };

            writeln!(f)?;
            write!(f, "  * {short_caller}")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_counter_from_miniperf_counter() {
        let counter = CpuCounter::from(MiniperfCounter {
            count: 123,
            time_enabled: 100,
            time_running: 50,
        });

        assert_eq!(counter.count, 123);
        assert_eq!(counter.time_enabled, 100);
        assert_eq!(counter.time_running, 50);
    }
}

/// Wire compatibility of `TestResultReuse` with the published fork schema (b1100380), and of the
/// merge's four renumbered fork-only numbers with binary logs the old fork wrote. Each fixture is
/// a verbatim copy of the old message definition, as prost would generate it.
#[cfg(test)]
mod schema_compat_tests {
    use prost::Message;

    use super::*;

    /// `TestResultReuse` exactly as fork b1100380 publishes it (fields 1–4 only).
    #[derive(Clone, PartialEq, prost::Message)]
    struct B1100380TestResultReuse {
        #[prost(string, tag = "1")]
        origin_attempt: String,
        #[prost(string, tag = "2")]
        entry_manifest: String,
        #[prost(string, tag = "3")]
        current_attempt: String,
        #[prost(message, optional, tag = "4")]
        current_execution_time: Option<prost_types::Duration>,
    }

    /// The unreleased r2/r3 candidate fields (never published), to show r4 reserves them.
    #[derive(Clone, PartialEq, prost::Message)]
    struct CandidateTestResultReuse {
        #[prost(string, tag = "1")]
        origin_attempt: String,
        #[prost(string, tag = "5")]
        action_digest: String,
        #[prost(bool, tag = "6")]
        served_execute: bool,
        #[prost(message, optional, tag = "7")]
        original_execution_time: Option<prost_types::Duration>,
    }

    fn duration(ms: u64) -> Option<prost_types::Duration> {
        Some(std::time::Duration::from_millis(ms).try_into().unwrap())
    }

    #[test]
    fn b1100380_test_result_reuse_bytes_decode_with_unrecorded_provenance() {
        let legacy = B1100380TestResultReuse {
            origin_attempt: "origin".to_owned(),
            entry_manifest: "manifest".to_owned(),
            current_attempt: "current".to_owned(),
            current_execution_time: duration(0),
        };
        let decoded = TestResultReuse::decode(legacy.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            decoded,
            TestResultReuse {
                origin_attempt: "origin".to_owned(),
                entry_manifest: "manifest".to_owned(),
                current_attempt: "current".to_owned(),
                current_execution_time: duration(0),
                action_digest: String::new(),
                served_by: None,
                original_execution_time: None,
            }
        );
        // Never ACTION_CACHE (0), which a non-optional enum would have defaulted to.
        assert_eq!(decoded.served_by, None);
    }

    #[test]
    fn candidate_numbers_5_to_7_are_ignored_never_misread() {
        let candidate = CandidateTestResultReuse {
            origin_attempt: "origin".to_owned(),
            action_digest: "abc:12".to_owned(),
            served_execute: true,
            original_execution_time: duration(9000),
        };
        let decoded = TestResultReuse::decode(candidate.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.origin_attempt, "origin");
        assert_eq!(decoded.action_digest, "");
        assert_eq!(decoded.served_by, None);
        assert_eq!(decoded.original_execution_time, None);
    }

    #[test]
    fn current_test_result_reuse_round_trips_each_served_by() {
        for served_by in [
            None,
            Some(RemoteCommandOrigin::ActionCache),
            Some(RemoteCommandOrigin::ReServedFromCache),
            Some(RemoteCommandOrigin::ReDeduplicated),
        ] {
            let reuse = TestResultReuse {
                current_execution_time: duration(250),
                action_digest: "abc:12".to_owned(),
                served_by: served_by.map(|o| o as i32),
                original_execution_time: duration(9000),
                ..Default::default()
            };
            let bytes = reuse.encode_to_vec();
            let decoded = TestResultReuse::decode(bytes.as_slice()).unwrap();
            assert_eq!(decoded, reuse, "{served_by:?}");
            assert_eq!(decoded.served_by(), served_by.unwrap_or_default());
            // Presence keeps an explicit ACTION_CACHE (0) distinct from absent.
            assert_eq!(decoded.served_by.is_some(), served_by.is_some());
            // And the JSON the event log writes agrees.
            let json = serde_json::to_string(&reuse).unwrap();
            let from_json: TestResultReuse = serde_json::from_str(&json).unwrap();
            assert_eq!(from_json, reuse, "{json}");
        }
    }

    // ---- The merge's renumbered fork-only numbers, against binary logs the old fork wrote ----

    /// Old fork (b1100380) `InvocationRecord.run_local_test_result_cache_count = 761`.
    #[derive(Clone, PartialEq, prost::Message)]
    struct OldForkInvocationRecord {
        #[prost(uint64, optional, tag = "761")]
        run_local_test_result_cache_count: Option<u64>,
    }

    /// Old fork `ExecutorStageStart` with `stage` set to one of its fork-only members (a set
    /// oneof member encodes exactly as this optional field).
    #[derive(Clone, PartialEq, prost::Message)]
    struct OldForkExecutorStageStart {
        #[prost(message, optional, tag = "25")]
        local_test_result_cache_query: Option<LocalTestResultCacheQuery>,
        #[prost(message, optional, tag = "26")]
        local_test_result_cache: Option<LocalTestResultCache>,
    }

    /// Old fork `ActionExecutionEnd.execution_kind = 22` / `CriticalPathEntry2.ActionExecution
    /// .execution_kind = 5`, where 12 was `ACTION_EXECUTION_KIND_LOCAL_TEST_RESULT_CACHE`.
    #[derive(Clone, PartialEq, prost::Message)]
    struct OldForkActionExecutionEnd {
        #[prost(int32, tag = "22")]
        execution_kind: i32,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct OldForkCriticalPathActionExecution {
        #[prost(int32, tag = "5")]
        execution_kind: i32,
    }
    const OLD_FORK_LOCAL_TEST_RESULT_CACHE: i32 = 12;

    #[test]
    fn old_fork_run_local_test_result_cache_count_is_a_decode_error() {
        for count in [0, 5] {
            let old = OldForkInvocationRecord {
                run_local_test_result_cache_count: Some(count),
            };
            // Merged 761 is `repeated string re_exceeded_quotas`: a varint there is the wrong
            // wire type, so the whole InvocationRecord fails to decode.
            let err = InvocationRecord::decode(old.encode_to_vec().as_slice()).unwrap_err();
            assert!(err.to_string().contains("wire type"), "{err}");
        }
        // Absent (never set) writes nothing and decodes.
        let old = OldForkInvocationRecord {
            run_local_test_result_cache_count: None,
        };
        let decoded = InvocationRecord::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(decoded.re_exceeded_quotas.is_empty());
        assert_eq!(decoded.run_local_test_result_cache_count, None);
    }

    #[test]
    fn old_fork_stage_25_is_a_decode_error_or_a_cas_artifact_misread() {
        // Merged 25 is `CasArtifactStage cas_artifact` (field 1 an enum, varint). The old
        // query's field 1 is the action digest string: wrong wire type, decode error.
        let old = OldForkExecutorStageStart {
            local_test_result_cache_query: Some(LocalTestResultCacheQuery {
                action_digest: "abc:12".to_owned(),
                cache_key: "key".to_owned(),
                current_attempt: "attempt".to_owned(),
            }),
            local_test_result_cache: None,
        };
        let err = ExecutorStageStart::decode(old.encode_to_vec().as_slice()).unwrap_err();
        assert!(err.to_string().contains("wire type"), "{err}");
        // With an empty digest (proto3 omits it), the remaining fields are unknown to
        // CasArtifactStage and skipped: it silently reads as a cas_artifact stage.
        let old = OldForkExecutorStageStart {
            local_test_result_cache_query: Some(LocalTestResultCacheQuery {
                action_digest: String::new(),
                cache_key: "key".to_owned(),
                current_attempt: "attempt".to_owned(),
            }),
            local_test_result_cache: None,
        };
        let decoded = ExecutorStageStart::decode(old.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            decoded.stage,
            Some(executor_stage_start::Stage::CasArtifact(
                CasArtifactStage::default()
            ))
        );
    }

    #[test]
    fn old_fork_stage_26_is_silently_dropped() {
        let old = OldForkExecutorStageStart {
            local_test_result_cache_query: None,
            local_test_result_cache: Some(LocalTestResultCache {
                action_digest: "abc:12".to_owned(),
                cache_key: "key".to_owned(),
                origin_attempt: "origin".to_owned(),
                entry_manifest: "manifest".to_owned(),
                current_attempt: "current".to_owned(),
            }),
        };
        // Merged ExecutorStageStart has no 26: an unknown field, skipped; the stage is lost.
        let decoded = ExecutorStageStart::decode(old.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.stage, None);
    }

    #[test]
    fn old_fork_execution_kind_12_silently_reads_as_remote_served_from_cache() {
        let end = ActionExecutionEnd::decode(
            OldForkActionExecutionEnd {
                execution_kind: OLD_FORK_LOCAL_TEST_RESULT_CACHE,
            }
            .encode_to_vec()
            .as_slice(),
        )
        .unwrap();
        assert_eq!(
            end.execution_kind(),
            ActionExecutionKind::RemoteServedFromCache
        );
        let entry = critical_path_entry2::ActionExecution::decode(
            OldForkCriticalPathActionExecution {
                execution_kind: OLD_FORK_LOCAL_TEST_RESULT_CACHE,
            }
            .encode_to_vec()
            .as_slice(),
        )
        .unwrap();
        assert_eq!(
            entry.execution_kind(),
            ActionExecutionKind::RemoteServedFromCache
        );
        // The JSON log writes this enum as its number too (only `ActionKind` is written by
        // name), so an old JSON line misreads the same way.
        let json = serde_json::to_string(&ActionExecutionEnd {
            execution_kind: OLD_FORK_LOCAL_TEST_RESULT_CACHE,
            ..Default::default()
        })
        .unwrap();
        assert!(json.contains("\"execution_kind\":12"), "{json}");
        let from_json: ActionExecutionEnd = serde_json::from_str(&json).unwrap();
        assert_eq!(
            from_json.execution_kind(),
            ActionExecutionKind::RemoteServedFromCache
        );
        assert_eq!(ActionExecutionKind::LocalTestResultCache as i32, 10001);
    }

    #[test]
    fn json_keys_the_renumbered_fields_by_name() {
        // Field numbers do not appear in the JSON log, which keys messages, fields and oneof
        // members by name; these three renumbered members keep their names.
        let stage = ExecutorStageStart {
            stage: Some(executor_stage_start::Stage::LocalTestResultCacheQuery(
                LocalTestResultCacheQuery {
                    action_digest: "abc:12".to_owned(),
                    ..Default::default()
                },
            )),
        };
        let json = serde_json::to_string(&stage).unwrap();
        assert!(!json.contains("10001"), "{json}");
        assert_eq!(
            serde_json::from_str::<ExecutorStageStart>(&json).unwrap(),
            stage
        );
        let record = InvocationRecord {
            run_local_test_result_cache_count: Some(5),
            ..Default::default()
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains("\"run_local_test_result_cache_count\":5"),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<InvocationRecord>(&json)
                .unwrap()
                .run_local_test_result_cache_count,
            Some(5)
        );
    }
}
