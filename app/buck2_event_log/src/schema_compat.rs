/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Fork-local: reading event logs across the fork's 2026-11-01 upstream sync.
//!
//! Before the sync, the fork (up to and including b1100380) wrote four fork-only numbers that
//! upstream later took for something else:
//!
//! | b1100380 (fork, pre-sync)                                          | upstream / current at that number          | current fork number |
//! |--------------------------------------------------------------------|--------------------------------------------|---------------------|
//! | `ActionExecutionKind` 12 `LOCAL_TEST_RESULT_CACHE`                 | 12 `REMOTE_SERVED_FROM_CACHE`              | 10001               |
//! | `ExecutorStageStart` 25 `LocalTestResultCacheQuery`                | 25 `CasArtifactStage cas_artifact`         | 10001               |
//! | `ExecutorStageStart` 26 `LocalTestResultCache`                     | (not defined)                              | 10002               |
//! | `InvocationRecord` 761 `optional uint64 run_local_test_result_cache_count` | 761 `repeated string re_exceeded_quotas` | 10001       |
//!
//! Current writers mark the log header with `Invocation.muninn_event_schema =`
//! [`MUNINN_EVENT_SCHEMA_CURRENT`]. A marked log is read with the current schema, as is. A log
//! without the marker was written either by upstream buck2 or by the fork before the marker
//! existed, and its bytes cannot in general tell which. For an unmarked log the reader therefore
//! applies these rules, and never asserts a meaning the bytes do not decide:
//!
//! - **Decided by the bytes, in either reading mode.** A b1100380 record that the current schema
//!   cannot decode (761 as a varint; 25 whose payload is valid only as b1100380's query, e.g. with
//!   a digest) is decoded with the b1100380 definitions in [`b1100380`] and translated to the
//!   current numbers. Upstream could not have written it. A 25 whose payload is valid only as
//!   `CasArtifactStage` (it has a phase) is upstream's and is left as is.
//! - **Ambiguous** ([`UnmarkedLogReading::Ambiguous`], the default): `ActionExecutionKind` 12
//!   becomes `SERVED_FROM_CACHE_OR_LEGACY_LOCAL_TEST_RESULT_CACHE`; a 25 valid under both schemas
//!   (e.g. empty) and any 26 become `ExecutorStageStart.unmarked_log_stage`, which carries each
//!   valid reading without choosing one.
//! - **Explicit legacy opt-in** ([`UnmarkedLogReading::LegacyFork`]): the caller states that the
//!   log was written by the fork at or before b1100380, so 12 is `LOCAL_TEST_RESULT_CACHE`, and 25
//!   and 26 are the local test-result cache query and hit, translated to 10001 and 10002.
//!
//! The stage rules apply to the member protobuf selects, the last oneof member on the wire: a 25
//! or 26 followed by another member is not a 25 or 26 stage. If only b1100380 can decode such a
//! message and it selects a member both schemas share, that member is taken. When 26 is selected,
//! the current schema skips it and may select an earlier member instead; the ambiguous reading
//! then carries that stage too (`UnmarkedLogStage.as_current_schema_stage`).
//!
//! The rules apply to each message as protobuf decodes it. Protobuf merges repeated occurrences of
//! a message field, until another member of the same oneof intervenes, so such occurrences are
//! read as the one merged message: a stage split across two occurrences of
//! `SpanStartEvent.executor_stage`, say 24 in one and 26 in the next, is the stage 24 then 26.
//!
//! If a translated record still fails to decode, the error of the original record is returned.
//!
//! JSON logs key fields and oneof members by name, so 25, 26 and 761 need no translation there;
//! `ActionExecutionKind` is written as its number, so the 12 rule applies to JSON too.

use buck2_data::ActionExecutionKind;
use buck2_data::executor_stage_start::Stage;
use buck2_event_log_format::stream_value::StreamValue;
use dupe::Dupe;
use prost::Message;
use prost::bytes::BytesMut;
use prost::encoding::WireType;

use crate::utils::Invocation;

/// The event schema current writers record in `Invocation.muninn_event_schema`: the fork-only
/// numbers are at 10001 and up, and upstream's numbers mean what upstream says. Logs without the
/// marker are "unmarked". `1` was never written; values below 2 are treated as unmarked.
pub const MUNINN_EVENT_SCHEMA_CURRENT: u32 = 2;

/// How to read a log without the `muninn_event_schema` marker.
#[derive(Copy, Clone, Dupe, Debug, Default, PartialEq, Eq)]
pub enum UnmarkedLogReading {
    /// Assert neither meaning where the bytes do not decide; expose the ambiguity instead.
    #[default]
    Ambiguous,
    /// Explicit opt-in: the log is known to have been written by this fork at or before
    /// b1100380, so its fork-only numbers have their pre-sync meaning.
    LegacyFork,
}

/// The schema a log was written with, from its header.
#[derive(Copy, Clone, Dupe, Debug, PartialEq, Eq)]
pub enum LogSchema {
    /// Marked by a current writer (`muninn_event_schema >= 2`).
    Marked(u32),
    /// No marker: written by upstream buck2, or by the fork before the marker existed.
    Unmarked,
}

impl LogSchema {
    pub fn of(muninn_event_schema: Option<u32>) -> Self {
        match muninn_event_schema {
            Some(v) if v >= MUNINN_EVENT_SCHEMA_CURRENT => LogSchema::Marked(v),
            _ => LogSchema::Unmarked,
        }
    }
}

/// Decodes the events of one log under the rules for its schema (see the module docs).
#[derive(Copy, Clone, Dupe, Debug)]
pub struct EventDecoder {
    /// `None` for a marked log.
    unmarked: Option<UnmarkedLogReading>,
}

impl EventDecoder {
    pub fn new(invocation: &Invocation, reading: UnmarkedLogReading) -> Self {
        Self {
            unmarked: match invocation.schema() {
                LogSchema::Marked(_) => None,
                LogSchema::Unmarked => Some(reading),
            },
        }
    }

    /// Decodes one length-delimited frame of a protobuf log.
    pub fn protobuf_frame(&self, frame: BytesMut) -> buck2_error::Result<StreamValue> {
        let Some(reading) = self.unmarked else {
            return buck2_event_log_format::decode::protobuf_frame(frame);
        };
        let mut value = match translate_protobuf_frame(&frame, reading) {
            None => buck2_event_log_format::decode::protobuf_frame(frame)?,
            Some(translated) => {
                match buck2_event_log_format::decode::protobuf_frame(BytesMut::from(
                    &translated[..],
                )) {
                    Ok(value) => value,
                    // Surface the original record's error, if it has one; never fall back to
                    // its (mis)reading.
                    Err(translated_err) => {
                        return Err(
                            match buck2_event_log_format::decode::protobuf_frame(frame) {
                                Err(original_err) => original_err,
                                Ok(_) => translated_err,
                            },
                        );
                    }
                }
            }
        };
        resolve_execution_kinds(&mut value, reading);
        Ok(value)
    }

    /// Decodes one line of a JSON log.
    pub fn json_line(&self, line: &str) -> serde_json::Result<StreamValue> {
        let mut value = buck2_event_log_format::decode::json_line(line)?;
        if let Some(reading) = self.unmarked {
            resolve_execution_kinds(&mut value, reading);
        }
        Ok(value)
    }
}

/// The fork's pre-sync (b1100380) definitions of the messages whose numbers changed, exactly as
/// prost generates them from b1100380's `app/buck2_data/data.proto`. Read-only: used to decode
/// unmarked logs, never to write.
///
/// Every other field of every message on a path to these was compared, by number, name and type,
/// between b1100380 and the current schema: they agree, except `RemoteCommand.cache_hit_type = 5`,
/// whose enum was renamed (`CacheHitType` to `RemoteCommandOrigin`) with the same values 0-2.
pub mod b1100380 {
    use buck2_data::CacheHit;
    use buck2_data::CacheQuery;
    use buck2_data::LocalStage;
    use buck2_data::PrepareAction;
    use buck2_data::ReStage;

    /// b1100380 `enum ActionExecutionKind`.
    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        prost::Enumeration
    )]
    #[repr(i32)]
    pub enum ActionExecutionKind {
        NotSet = 0,
        Local = 1,
        Remote = 2,
        ActionCache = 3,
        Simple = 4,
        Deferred = 6,
        LocalDepFile = 7,
        LocalWorker = 8,
        RemoteDepFileCache = 9,
        LocalActionCache = 10,
        RemoteWorker = 11,
        LocalTestResultCache = 12,
    }

    /// b1100380 `message LocalTestResultCacheQuery`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct LocalTestResultCacheQuery {
        #[prost(string, tag = "1")]
        pub action_digest: String,
        #[prost(string, tag = "2")]
        pub cache_key: String,
        #[prost(string, tag = "3")]
        pub current_attempt: String,
    }

    /// b1100380 `message LocalTestResultCache`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct LocalTestResultCache {
        #[prost(string, tag = "1")]
        pub action_digest: String,
        #[prost(string, tag = "2")]
        pub cache_key: String,
        #[prost(string, tag = "3")]
        pub origin_attempt: String,
        #[prost(string, tag = "4")]
        pub entry_manifest: String,
        #[prost(string, tag = "5")]
        pub current_attempt: String,
    }

    /// b1100380 `message ExecutorStageStart`. Members 20-24 are unchanged in the current schema.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ExecutorStageStart {
        #[prost(
            oneof = "executor_stage_start::Stage",
            tags = "20, 21, 22, 23, 24, 25, 26"
        )]
        pub stage: Option<executor_stage_start::Stage>,
    }

    pub mod executor_stage_start {
        use super::*;

        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Stage {
            #[prost(message, tag = "20")]
            Re(ReStage),
            #[prost(message, tag = "21")]
            Local(LocalStage),
            #[prost(message, tag = "22")]
            CacheQuery(CacheQuery),
            #[prost(message, tag = "23")]
            CacheHit(CacheHit),
            #[prost(message, tag = "24")]
            Prepare(PrepareAction),
            #[prost(message, tag = "25")]
            LocalTestResultCacheQuery(super::LocalTestResultCacheQuery),
            #[prost(message, tag = "26")]
            LocalTestResultCache(super::LocalTestResultCache),
        }
    }

    /// b1100380 `InvocationRecord.run_local_test_result_cache_count = 761`, its only field whose
    /// number means something else in the current schema. The other fields agree, so the rest of
    /// a b1100380 `InvocationRecord` is decoded with the current type.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct InvocationRecordForkFields {
        #[prost(uint64, optional, tag = "761")]
        pub run_local_test_result_cache_count: Option<u64>,
    }

    impl From<LocalTestResultCacheQuery> for buck2_data::LocalTestResultCacheQuery {
        fn from(v: LocalTestResultCacheQuery) -> Self {
            buck2_data::LocalTestResultCacheQuery {
                action_digest: v.action_digest,
                cache_key: v.cache_key,
                current_attempt: v.current_attempt,
            }
        }
    }

    impl From<LocalTestResultCache> for buck2_data::LocalTestResultCache {
        fn from(v: LocalTestResultCache) -> Self {
            buck2_data::LocalTestResultCache {
                action_digest: v.action_digest,
                cache_key: v.cache_key,
                origin_attempt: v.origin_attempt,
                entry_manifest: v.entry_manifest,
                current_attempt: v.current_attempt,
            }
        }
    }
}

/// Field numbers from a `CommandProgress` frame to the messages that b1100380 numbered
/// differently. The paths are the same in b1100380 and the current schema, and these are the
/// only paths to those messages.
mod path {
    use prost::Message;

    /// One field on a path: its number, and the current type of the message that contains it.
    pub(super) struct Step {
        pub(super) number: u32,
        /// Decodes a message of that type as the current schema does, and encodes it again.
        /// `None` if it does not decode.
        pub(super) reencode: fn(&[u8]) -> Option<Vec<u8>>,
    }

    fn reencode<M: Message + Default>(bytes: &[u8]) -> Option<Vec<u8>> {
        M::decode(bytes).ok().map(|m| m.encode_to_vec())
    }

    const EVENT: Step = Step {
        number: 1,
        reencode: reencode::<buck2_cli_proto::CommandProgress>,
    };

    /// `CommandProgress.event = 1` > `BuckEvent.span_start = 20` >
    /// `SpanStartEvent.executor_stage = 54`.
    pub(super) const EXECUTOR_STAGE_START: &[Step] = &[
        EVENT,
        Step {
            number: 20,
            reencode: reencode::<buck2_data::BuckEvent>,
        },
        Step {
            number: 54,
            reencode: reencode::<buck2_data::SpanStartEvent>,
        },
    ];
    /// `CommandProgress.event = 1` > `BuckEvent.record = 23` >
    /// `RecordEvent.invocation_record = 1`.
    pub(super) const INVOCATION_RECORD: &[Step] = &[
        EVENT,
        Step {
            number: 23,
            reencode: reencode::<buck2_data::BuckEvent>,
        },
        Step {
            number: 1,
            reencode: reencode::<buck2_data::RecordEvent>,
        },
    ];
    /// `ExecutorStageStart` stages b1100380 numbered differently.
    pub(super) const STAGE_25: u32 = 25;
    pub(super) const STAGE_26: u32 = 26;
    /// b1100380's `ExecutorStageStart.stage` oneof members.
    pub(super) const STAGES_B1100380: std::ops::RangeInclusive<u32> = 20..=26;
    /// `InvocationRecord.run_local_test_result_cache_count` in b1100380.
    pub(super) const RECORD_761: u32 = 761;
}

/// One field of an encoded message.
struct Field<'a> {
    number: u32,
    wire_type: WireType,
    /// The whole field: key and value.
    raw: &'a [u8],
    /// The payload of a length-delimited field (otherwise unused).
    payload: &'a [u8],
}

/// Splits an encoded message into its fields. `None` if it is malformed or uses groups; such a
/// message is left to the ordinary decoder, which reports it.
fn fields(mut buf: &[u8]) -> Option<Vec<Field<'_>>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let start = buf;
        let (number, wire_type) = prost::encoding::decode_key(&mut buf).ok()?;
        let mut payload: &[u8] = &[];
        match wire_type {
            WireType::Varint => {
                prost::encoding::decode_varint(&mut buf).ok()?;
            }
            WireType::SixtyFourBit => buf = buf.get(8..)?,
            WireType::ThirtyTwoBit => buf = buf.get(4..)?,
            WireType::LengthDelimited => {
                let len = usize::try_from(prost::encoding::decode_varint(&mut buf).ok()?).ok()?;
                payload = buf.get(..len)?;
                buf = &buf[len..];
            }
            WireType::StartGroup | WireType::EndGroup => return None,
        }
        out.push(Field {
            number,
            wire_type,
            raw: &start[..start.len() - buf.len()],
            payload,
        });
    }
    Some(out)
}

/// Applies `leaf` to every message reached by `path` from `msg`, re-encoding the enclosing
/// messages where a leaf changes. `None` if nothing changed.
///
/// A message is what protobuf decodes at a step: the payloads of the step's occurrences that it
/// merges, concatenated (see [`merged_occurrences`]). Where a leaf changes one, its occurrences
/// are replaced by a single occurrence, at the first one's position.
fn rewrite_at(
    msg: &[u8],
    path: &[path::Step],
    leaf: &mut dyn FnMut(&[u8]) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let Some((step, rest)) = path.split_first() else {
        return leaf(msg);
    };
    let fields = fields(msg)?;
    // For each field, the rewritten message to write in its place (`Some(Some(_))`), or that it is
    // folded into an earlier occurrence (`Some(None)`). Other fields are copied as they are.
    let mut replace: Vec<Option<Option<Vec<u8>>>> = vec![None; fields.len()];
    for run in merged_occurrences(&fields, step) {
        let payload: Vec<u8> = run
            .iter()
            .flat_map(|&i| fields[i].payload)
            .copied()
            .collect();
        if let Some(new) = rewrite_at(&payload, rest, leaf) {
            let mut field = Vec::with_capacity(new.len() + 8);
            prost::encoding::encode_key(step.number, WireType::LengthDelimited, &mut field);
            prost::encoding::encode_varint(new.len() as u64, &mut field);
            field.extend_from_slice(&new);
            replace[run[0]] = Some(Some(field));
            for &i in &run[1..] {
                replace[i] = Some(None);
            }
        }
    }
    if replace.iter().all(Option::is_none) {
        return None;
    }
    let mut out = Vec::with_capacity(msg.len() + 16);
    for (field, replacement) in fields.iter().zip(&replace) {
        match replacement {
            None => out.extend_from_slice(field.raw),
            Some(Some(new)) => out.extend_from_slice(new),
            Some(None) => {}
        }
    }
    Some(out)
}

/// The occurrences of `step.number` (length-delimited) among `fields` that protobuf merges into
/// one message, as runs of indices into `fields`. A run ends where another member of the same
/// oneof occurs: protobuf then starts the next occurrence afresh.
fn merged_occurrences(fields: &[Field<'_>], step: &path::Step) -> Vec<Vec<usize>> {
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut open = false;
    for (i, field) in fields.iter().enumerate() {
        if field.number == step.number && field.wire_type == WireType::LengthDelimited {
            match runs.last_mut() {
                Some(run) if open => run.push(i),
                _ => runs.push(vec![i]),
            }
            open = true;
        } else if open && ends_message(step, field) {
            open = false;
        }
    }
    runs
}

/// Whether `field`, after an occurrence of `step.number`, ends the message that occurrence holds:
/// asks the current schema's decoder of the containing message whether `step.number` survives
/// an empty occurrence of it followed by `field`. It does not when `field` is another member of
/// the same oneof. If that does not decode (as for `step.number` itself, not length-delimited),
/// it is taken to end the message, so the occurrences are read separately.
fn ends_message(step: &path::Step, field: &Field<'_>) -> bool {
    let mut probe = Vec::with_capacity(field.raw.len() + 8);
    prost::encoding::encode_key(step.number, WireType::LengthDelimited, &mut probe);
    prost::encoding::encode_varint(0, &mut probe);
    probe.extend_from_slice(field.raw);
    match (step.reencode)(&probe).as_deref().and_then(fields) {
        Some(decoded) => !decoded.iter().any(|f| f.number == step.number),
        None => true,
    }
}

/// Translates the b1100380-numbered parts of one length-delimited frame of an unmarked log to
/// the current schema. `None` if the frame has none, or cannot be split into fields.
fn translate_protobuf_frame(frame: &[u8], reading: UnmarkedLogReading) -> Option<Vec<u8>> {
    let mut body = frame;
    let len = prost::decode_length_delimiter(&mut body).ok()?;
    let body = body.get(..len)?;

    let stage = rewrite_at(body, path::EXECUTOR_STAGE_START, &mut |payload| {
        translate_executor_stage_start(payload, reading)
    });
    let body = stage.as_deref().unwrap_or(body);
    let record = rewrite_at(
        body,
        path::INVOCATION_RECORD,
        &mut translate_invocation_record,
    );
    if stage.is_none() && record.is_none() {
        return None;
    }
    let body = record.as_deref().unwrap_or(body);
    let mut out = Vec::with_capacity(body.len() + 5);
    prost::encoding::encode_varint(body.len() as u64, &mut out);
    out.extend_from_slice(body);
    Some(out)
}

/// See the module docs. `None` leaves the stage as the current schema reads it.
///
/// The rules apply to the stage member protobuf selects: oneof members are last-wins on the
/// wire, so an earlier 25 or 26 followed by another member does not make the stage a 25 or 26.
fn translate_executor_stage_start(payload: &[u8], reading: UnmarkedLogReading) -> Option<Vec<u8>> {
    use b1100380::executor_stage_start::Stage as Legacy;

    // The member b1100380 selects: the last of its oneof's numbers (20-26) on the wire. The
    // current schema selects the same one, except that it skips a 26.
    let selected = fields(payload)?
        .iter()
        .rev()
        .map(|f| f.number)
        .find(|n| path::STAGES_B1100380.contains(n));
    let current = buck2_data::ExecutorStageStart::decode(payload);
    let legacy = b1100380::ExecutorStageStart::decode(payload)
        .ok()
        .and_then(|l| l.stage);
    let unmarked = |field_number| buck2_data::UnmarkedLogStage {
        field_number,
        ..Default::default()
    };
    let stage = match legacy {
        // b1100380 selects a member (20-24) that both schemas share; its decoding is last-wins
        // like `selected`, so the selected member is neither 25 nor 26.
        Some(
            Legacy::Re(_)
            | Legacy::Local(_)
            | Legacy::CacheQuery(_)
            | Legacy::CacheHit(_)
            | Legacy::Prepare(_),
        ) => match (current, legacy) {
            // Both schemas read the message, and select the same member: leave it.
            (Ok(_), _) => return None,
            // Only b1100380 reads the message (an earlier member is valid only there, e.g. a
            // 25 with a digest): decided by the bytes, so take its selected member.
            (Err(_), Some(Legacy::Re(s))) => Stage::Re(s),
            (Err(_), Some(Legacy::Local(s))) => Stage::Local(s),
            (Err(_), Some(Legacy::CacheQuery(s))) => Stage::CacheQuery(s),
            (Err(_), Some(Legacy::CacheHit(s))) => Stage::CacheHit(s),
            (Err(_), Some(Legacy::Prepare(s))) => Stage::Prepare(s),
            (Err(_), _) => return None,
        },
        Some(Legacy::LocalTestResultCacheQuery(query)) => {
            let query: buck2_data::LocalTestResultCacheQuery = query.into();
            match (current, reading) {
                // Valid only under b1100380, or the caller says the log is the fork's.
                (Err(_), _) | (_, UnmarkedLogReading::LegacyFork) => {
                    Stage::LocalTestResultCacheQuery(query)
                }
                (Ok(current), UnmarkedLogReading::Ambiguous) => {
                    Stage::UnmarkedLogStage(Box::new(buck2_data::UnmarkedLogStage {
                        as_cas_artifact: match current.stage {
                            Some(Stage::CasArtifact(cas)) => Some(cas),
                            _ => None,
                        },
                        as_legacy_local_test_result_cache_query: Some(query),
                        ..unmarked(path::STAGE_25)
                    }))
                }
            }
        }
        Some(Legacy::LocalTestResultCache(hit)) => {
            let hit: buck2_data::LocalTestResultCache = hit.into();
            match reading {
                UnmarkedLogReading::LegacyFork => Stage::LocalTestResultCache(hit),
                UnmarkedLogReading::Ambiguous => {
                    Stage::UnmarkedLogStage(Box::new(buck2_data::UnmarkedLogStage {
                        as_legacy_local_test_result_cache: Some(hit),
                        as_current_schema_stage: current_reading(current),
                        ..unmarked(path::STAGE_26)
                    }))
                }
            }
        }
        // A selected 26 that is not b1100380's either: not represented in the current schema,
        // and noted as such rather than dropped silently, with what the current schema reads.
        _ if selected == Some(path::STAGE_26) => {
            Stage::UnmarkedLogStage(Box::new(buck2_data::UnmarkedLogStage {
                as_current_schema_stage: current_reading(current),
                ..unmarked(path::STAGE_26)
            }))
        }
        // The same under both schemas, or valid only as the current schema reads it.
        _ => return None,
    };
    Some(buck2_data::ExecutorStageStart { stage: Some(stage) }.encode_to_vec())
}

/// The current schema's reading of a stage message whose selected member is 26, which it does
/// not define: the stage it selects instead, if any. (Boxed: the message contains itself.)
fn current_reading(
    current: Result<buck2_data::ExecutorStageStart, prost::DecodeError>,
) -> Option<Box<buck2_data::ExecutorStageStart>> {
    current.ok().filter(|c| c.stage.is_some()).map(Box::new)
}

/// A varint at 761 is b1100380's `run_local_test_result_cache_count`; upstream's 761 is
/// length-delimited. Moves it to the current field. `None` leaves the record as is.
fn translate_invocation_record(payload: &[u8]) -> Option<Vec<u8>> {
    let fields = fields(payload)?;
    if !fields
        .iter()
        .any(|f| f.number == path::RECORD_761 && f.wire_type == WireType::Varint)
    {
        return None;
    }
    // Fails if 761 also occurs length-delimited: that is no b1100380 record, so leave it to the
    // ordinary decoder to report.
    let legacy = b1100380::InvocationRecordForkFields::decode(payload).ok()?;
    let mut out = Vec::with_capacity(payload.len());
    for field in fields.iter().filter(|f| f.number != path::RECORD_761) {
        out.extend_from_slice(field.raw);
    }
    buck2_data::InvocationRecord {
        run_local_test_result_cache_count: legacy.run_local_test_result_cache_count,
        ..Default::default()
    }
    .encode(&mut out)
    .ok()?;
    Some(out)
}

/// `ActionExecutionKind` 12 in an unmarked log, wherever an event carries the enum.
fn resolve_execution_kinds(value: &mut StreamValue, reading: UnmarkedLogReading) {
    use buck2_data::buck_event::Data;

    const AMBIGUOUS: i32 = b1100380::ActionExecutionKind::LocalTestResultCache as i32;
    let resolved = match reading {
        UnmarkedLogReading::Ambiguous => {
            ActionExecutionKind::ServedFromCacheOrLegacyLocalTestResultCache
        }
        UnmarkedLogReading::LegacyFork => ActionExecutionKind::LocalTestResultCache,
    } as i32;
    let resolve = |kind: &mut i32| {
        if *kind == AMBIGUOUS {
            *kind = resolved;
        }
    };

    let StreamValue::Event(event) = value else {
        return;
    };
    match &mut event.data {
        Some(Data::SpanEnd(end)) => {
            if let Some(buck2_data::span_end_event::Data::ActionExecution(action)) = &mut end.data {
                resolve(&mut action.execution_kind);
            }
        }
        Some(Data::Instant(instant)) => {
            if let Some(buck2_data::instant_event::Data::BuildGraphInfo(info)) = &mut instant.data {
                for entry in info
                    .critical_path2
                    .iter_mut()
                    .chain(info.slowest_path.iter_mut())
                {
                    if let Some(buck2_data::critical_path_entry2::Entry::ActionExecution(action)) =
                        &mut entry.entry
                    {
                        resolve(&mut action.execution_kind);
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paths_reach_the_current_messages() {
        // Encode with the current types and find each message along its path.
        let stage = buck2_data::ExecutorStageStart {
            stage: Some(Stage::Prepare(buck2_data::PrepareAction {})),
        };
        let progress = buck2_cli_proto::CommandProgress {
            progress: Some(buck2_cli_proto::command_progress::Progress::Event(
                Box::new(buck2_data::BuckEvent {
                    data: Some(buck2_data::buck_event::Data::SpanStart(
                        buck2_data::SpanStartEvent {
                            data: Some(stage.clone().into()),
                        },
                    )),
                    ..Default::default()
                }),
            )),
        };
        let mut seen = Vec::new();
        rewrite_at(
            &progress.encode_to_vec(),
            path::EXECUTOR_STAGE_START,
            &mut |payload| {
                seen.push(payload.to_vec());
                None
            },
        );
        assert_eq!(seen, vec![stage.encode_to_vec()]);

        let record = buck2_data::InvocationRecord {
            run_local_count: 3,
            ..Default::default()
        };
        let progress = buck2_cli_proto::CommandProgress {
            progress: Some(buck2_cli_proto::command_progress::Progress::Event(
                Box::new(buck2_data::BuckEvent {
                    data: Some(buck2_data::buck_event::Data::Record(
                        buck2_data::RecordEvent {
                            data: Some(buck2_data::record_event::Data::InvocationRecord(Box::new(
                                record.clone(),
                            ))),
                        },
                    )),
                    ..Default::default()
                }),
            )),
        };
        let mut seen = Vec::new();
        rewrite_at(
            &progress.encode_to_vec(),
            path::INVOCATION_RECORD,
            &mut |payload| {
                seen.push(payload.to_vec());
                None
            },
        );
        assert_eq!(seen, vec![record.encode_to_vec()]);
    }

    #[test]
    fn the_legacy_and_current_stage_numbers_are_what_the_table_says() {
        let q = |s: Stage| buck2_data::ExecutorStageStart { stage: Some(s) }.encode_to_vec();
        let number = |bytes: Vec<u8>| fields(&bytes).unwrap()[0].number;
        assert_eq!(
            number(q(Stage::CasArtifact(
                buck2_data::CasArtifactStage::default()
            ))),
            25
        );
        assert_eq!(
            number(q(Stage::LocalTestResultCacheQuery(Default::default()))),
            10001
        );
        assert_eq!(
            number(q(Stage::LocalTestResultCache(Default::default()))),
            10002
        );
        assert_eq!(
            number(q(Stage::UnmarkedLogStage(Default::default()))),
            10003
        );
        let legacy = |s| b1100380::ExecutorStageStart { stage: Some(s) }.encode_to_vec();
        use b1100380::executor_stage_start::Stage as Legacy;
        assert_eq!(
            number(legacy(
                Legacy::LocalTestResultCacheQuery(Default::default())
            )),
            25
        );
        assert_eq!(
            number(legacy(Legacy::LocalTestResultCache(Default::default()))),
            26
        );
        assert_eq!(
            b1100380::ActionExecutionKind::LocalTestResultCache as i32,
            12
        );
        assert_eq!(ActionExecutionKind::RemoteServedFromCache as i32, 12);
        assert_eq!(ActionExecutionKind::LocalTestResultCache as i32, 10001);
    }
}
