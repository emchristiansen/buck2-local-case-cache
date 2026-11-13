/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::SystemTime;

use async_compression::tokio::bufread::GzipDecoder;
use async_compression::tokio::bufread::ZstdDecoder;
use buck2_cli_proto::protobuf_util::ProtobufSplitter;
use buck2_error::BuckErrorContext;
use buck2_error::BuckErrorOptionContext;
use buck2_error::ErrorTag;
use buck2_events::BuckEvent;
use buck2_fs::async_fs_util;
use buck2_fs::error::IoResultExt;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_fs::paths::abs_path::AbsPathBuf;
use buck2_wrapper_common::invocation_id::TraceId;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::stream::Stream;
use futures::stream::TryStreamExt;
use pin_project::pin_project;
use prost::Message;
use regex::Regex;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::BufReader;
use tokio::io::ReadBuf;
use tokio_stream::wrappers::LinesStream;
use tokio_util::codec::FramedRead;

use crate::schema_compat::EventDecoder;
use crate::schema_compat::UnmarkedLogReading;
use crate::stream_value::StreamValue;
use crate::tail::TailOptions;
use crate::tail::TailReader;
use crate::utils::Compression;
use crate::utils::Encoding;
use crate::utils::EventLogErrors;
use crate::utils::EventLogInferenceError;
use crate::utils::Invocation;
use crate::utils::KNOWN_ENCODINGS;
use crate::utils::LogMode;

type EventLogReader<'a> = Box<dyn AsyncRead + Send + Sync + Unpin + 'a>;

/// Check if an error is caused by a truncated compressed stream. This
/// can happen when reading in-progress event logs that don't have a
/// proper compression footer yet.
fn is_truncated_stream_error(error: &buck2_error::Error) -> bool {
    // Match exact error messages from compression-codecs library:
    // - zstd/bzip2/lz4: "... stream did not finish"
    // - gzip: "unexpected end of file" (default UnexpectedEof message)
    let msg = error.to_string().to_lowercase();
    msg.contains("did not finish") || msg.contains("unexpected end of file")
}

/// Wrap a stream to treat truncated compressed stream errors as EOF.
/// This allows reading in-progress event logs that may not have a
/// proper compression footer yet.
fn tolerant_of_truncation<T>(
    stream: impl Stream<Item = buck2_error::Result<T>>,
) -> impl Stream<Item = buck2_error::Result<T>> {
    stream.scan(false, |stopped, result| {
        if *stopped {
            return futures::future::ready(None);
        }
        match result {
            Ok(item) => futures::future::ready(Some(Ok(item))),
            Err(e) if is_truncated_stream_error(&e) => {
                // Treat truncation error as end of stream
                *stopped = true;
                futures::future::ready(None)
            }
            Err(e) => futures::future::ready(Some(Err(e))),
        }
    })
}

pub struct ReaderStats {
    compressed_bytes: AtomicUsize,
    decompressed_bytes: AtomicUsize,
}

impl ReaderStats {
    pub fn new() -> Self {
        Self {
            compressed_bytes: AtomicUsize::new(0),
            decompressed_bytes: AtomicUsize::new(0),
        }
    }

    pub fn compressed_bytes(&self) -> usize {
        self.compressed_bytes.load(Ordering::Relaxed)
    }

    pub fn decompressed_bytes(&self) -> usize {
        self.decompressed_bytes.load(Ordering::Relaxed)
    }
}

mod counting_reader {
    #![allow(clippy::ref_option_ref)] // for the projection

    use super::*;

    #[pin_project]
    pub struct CountingReader<'a, T> {
        #[pin]
        pub(super) inner: T,
        pub(super) stats: Option<&'a AtomicUsize>,
    }
}

use counting_reader::CountingReader;

impl<'a, T> CountingReader<'a, T> {
    fn new(inner: T, stats: Option<&'a AtomicUsize>) -> Self {
        Self { inner, stats }
    }
}

impl<T> AsyncRead for CountingReader<'_, T>
where
    T: AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.remaining();

        let this = self.project();
        futures::ready!(this.inner.poll_read(cx, buf))?;
        if let Some(stats) = this.stats.as_mut() {
            stats.fetch_add(before - buf.remaining(), Ordering::Relaxed);
        }

        Poll::Ready(Ok(()))
    }
}

#[derive(Clone)]
pub struct EventLogPathBuf {
    pub(crate) path: AbsPathBuf,
    pub(crate) encoding: Encoding,
    /// How to read the log if its header has no `muninn_event_schema` marker. See
    /// `crate::schema_compat`.
    pub(crate) unmarked_log_reading: UnmarkedLogReading,
}

pub struct EventLogSummary {
    pub trace_id: TraceId,
    pub timestamp: SystemTime,
    pub invocation: Invocation,
}

impl EventLogPathBuf {
    pub fn infer(path: AbsPathBuf) -> buck2_error::Result<Self> {
        match Self::infer_opt(&path)? {
            Some(v) => Ok(v),
            None => Err(EventLogInferenceError::InvalidExtension(path).into()),
        }
    }

    pub fn path(&self) -> &AbsPath {
        &self.path
    }

    /// Read the log, if its header has no `muninn_event_schema` marker, as `reading` says. The
    /// default, `UnmarkedLogReading::Ambiguous`, asserts no meaning that the log's bytes do not
    /// decide; `UnmarkedLogReading::LegacyFork` is the explicit opt-in for a log known to have
    /// been written by this fork at or before b1100380. A marked log is read the same either way.
    pub fn with_unmarked_log_reading(mut self, reading: UnmarkedLogReading) -> Self {
        self.unmarked_log_reading = reading;
        self
    }

    pub fn unmarked_log_reading(&self) -> UnmarkedLogReading {
        self.unmarked_log_reading
    }

    /// The decoder for this log's events, given its header: see `unpack_protobuf_frames`.
    pub fn event_decoder(&self, invocation: &Invocation) -> EventDecoder {
        EventDecoder::new(invocation, self.unmarked_log_reading)
    }

    fn file_name(path: &AbsPathBuf) -> buck2_error::Result<&str> {
        let name = path
            .file_name()
            .ok_or_else(|| EventLogInferenceError::NoFilename(path.clone()))?
            .to_str()
            .ok_or_else(|| EventLogInferenceError::InvalidFilename(path.clone()))?;
        Ok(name)
    }

    pub fn uuid_from_filename(&self) -> buck2_error::Result<TraceId> {
        let name = Self::file_name(&self.path)?;
        let re = Regex::new(
            r"([a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12})",
        )?;
        let uuid = re
            .find(name)
            .ok_or(EventLogInferenceError::NoUuidInFilename(self.path.clone()))?
            .as_str();
        TraceId::from_str(uuid).buck_error_context("Failed to create TraceId from uuid")
    }

    // TODO iguridi: this should be done by parsing file header
    pub fn command_from_filename(&self) -> buck2_error::Result<&str> {
        let file_name = Self::file_name(&self.path)?;
        // format is of the form "{ts}_{command}_{uuid}_events{ext}"
        match file_name.split('_').nth(1) {
            Some(command) => Ok(command),
            None => Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "No command in filename"
            )),
        }
    }

    pub(crate) fn infer_opt(path: &AbsPathBuf) -> buck2_error::Result<Option<Self>> {
        let name = Self::file_name(path)?;

        for encoding in KNOWN_ENCODINGS {
            for extension in encoding.extensions {
                if name.ends_with(extension) {
                    return Ok(Some(Self {
                        path: path.clone(),
                        encoding: *encoding,
                        unmarked_log_reading: UnmarkedLogReading::default(),
                    }));
                }
            }
        }

        Ok(None)
    }

    /// Not an `async fn`, and not inlinable: the stream built here pulls in the JSON
    /// deserialization of every event type. Rustc otherwise instantiates the body of an
    /// `async fn` anew in every downstream crate that awaits it, which costs each crate that
    /// reads logs tens of seconds of codegen. Boxing behind a non-inlined function compiles it
    /// once, here.
    #[inline(never)]
    fn unpack_stream_json<'a, 's>(
        &'s self,
        stats: Option<&'a ReaderStats>,
        tail: Option<TailOptions>,
    ) -> BoxFuture<
        's,
        buck2_error::Result<(Invocation, BoxStream<'a, buck2_error::Result<StreamValue>>)>,
    >
    where
        'a: 's,
    {
        Box::pin(async move {
            assert_eq!(self.encoding.mode, LogMode::Json);

            let log_file = self.open(stats, tail).await?;
            let log_file = BufReader::new(log_file);
            let mut log_lines = log_file.lines();

            // This one is not an event.
            let header = log_lines
                .next_line()
                .await
                .buck_error_context("Error reading header line")?
                .internal_error("No header line")?;
            let invocation = Invocation::parse_json_line(&header)?;
            let decoder = self.event_decoder(&invocation);

            let events = LinesStream::new(log_lines).map(move |line| {
                let line = line.buck_error_context("Error reading next line")?;
                decoder
                    .json_line(&line)
                    .with_buck_error_context(|| format!("Invalid line: {}", line.trim_end()))
            });

            // Wrap in tolerant_of_truncation to handle in-progress logs
            let events = tolerant_of_truncation(events);

            Ok((invocation, events.boxed()))
        })
    }

    /// Reads the invocation of a protobuf log, then returns the remaining length-delimited records
    /// undecoded.
    async fn protobuf_frames<'a>(
        &self,
        stats: Option<&'a ReaderStats>,
        tail: Option<TailOptions>,
    ) -> buck2_error::Result<(
        Invocation,
        impl Stream<Item = buck2_error::Result<prost::bytes::BytesMut>> + use<'a>,
    )> {
        assert_eq!(self.encoding.mode, LogMode::Protobuf);

        let log_file = self.open(stats, tail).await?;
        let mut stream = FramedRead::new(log_file, ProtobufSplitter);

        let invocation = stream
            .try_next()
            .await?
            .internal_error("No invocation found")?;
        let invocation = buck2_data::Invocation::decode_length_delimited(invocation)
            .buck_error_context("Invalid Invocation")?;
        Ok((Invocation::from_proto(invocation), stream))
    }

    /// See `unpack_stream_json` for why this is boxed and not inlinable.
    #[inline(never)]
    fn unpack_stream_protobuf<'a, 's>(
        &'s self,
        stats: Option<&'a ReaderStats>,
        tail: Option<TailOptions>,
    ) -> BoxFuture<
        's,
        buck2_error::Result<(Invocation, BoxStream<'a, buck2_error::Result<StreamValue>>)>,
    >
    where
        'a: 's,
    {
        Box::pin(async move {
            let (invocation, stream) = self.protobuf_frames(stats, tail).await?;
            let decoder = self.event_decoder(&invocation);

            let events = stream.and_then(move |data| async move { decoder.protobuf_frame(data) });

            // Wrap in tolerant_of_truncation to handle in-progress logs
            let events = tolerant_of_truncation(events);

            Ok((invocation, events.boxed()))
        })
    }

    async fn unpack_stream_inner<'a>(
        &self,
        stats: Option<&'a ReaderStats>,
        tail: Option<TailOptions>,
    ) -> buck2_error::Result<(
        Invocation,
        impl Stream<Item = buck2_error::Result<StreamValue>> + use<'a>,
    )> {
        match self.encoding.mode {
            LogMode::Json => self.unpack_stream_json(stats, tail).await,
            LogMode::Protobuf => self.unpack_stream_protobuf(stats, tail).await,
        }
    }

    /// Read the invocation line then the event stream.
    pub async fn unpack_stream_with_stats<'a>(
        &self,
        stats: &'a ReaderStats,
    ) -> buck2_error::Result<(
        Invocation,
        impl Stream<Item = buck2_error::Result<StreamValue>> + use<'a>,
    )> {
        self.unpack_stream_inner(Some(stats), None).await
    }

    pub async fn unpack_stream(
        &self,
    ) -> buck2_error::Result<(
        Invocation,
        impl Stream<Item = buck2_error::Result<StreamValue>> + 'static + use<>,
    )> {
        self.unpack_stream_inner(None, None).await
    }

    /// Whether the log is protobuf-encoded, which `unpack_protobuf_frames` requires.
    pub fn is_protobuf(&self) -> bool {
        self.encoding.mode == LogMode::Protobuf
    }

    /// Reads the invocation, then returns the remaining length-delimited protobuf records, in file
    /// order and without decoding them, for a caller that decodes them itself, for example on
    /// several threads. Fails for a log that isn't protobuf. Like `unpack_stream`, the records end
    /// without an error where a log that is still being written, or was cut off, ends.
    ///
    /// Decode the records with `event_decoder(&invocation).protobuf_frame`, which applies the
    /// rules for logs without a schema marker; decoding them directly does not.
    pub async fn unpack_protobuf_frames(
        &self,
    ) -> buck2_error::Result<(
        Invocation,
        BoxStream<'static, buck2_error::Result<prost::bytes::BytesMut>>,
    )> {
        if !self.is_protobuf() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Event log `{}` is not protobuf-encoded",
                self.path
            ));
        }
        let (invocation, stream) = self.protobuf_frames(None, None).await?;
        Ok((invocation, tolerant_of_truncation(stream).boxed()))
    }

    /// Like `unpack_stream`, but on reaching the end of a log that is still being written,
    /// waits for more events instead of ending the stream. The stream ends when the log is
    /// complete, or when it stops growing for `options.idle_timeout` (e.g. the writing
    /// command crashed, or deleted the log from under us).
    pub async fn unpack_stream_tailing(
        &self,
        options: TailOptions,
    ) -> buck2_error::Result<(
        Invocation,
        impl Stream<Item = buck2_error::Result<StreamValue>> + 'static + use<>,
    )> {
        self.unpack_stream_inner(None, Some(options)).await
    }

    async fn open<'a>(
        &self,
        stats: Option<&'a ReaderStats>,
        tail: Option<TailOptions>,
    ) -> buck2_error::Result<EventLogReader<'a>> {
        tracing::info!(
            "Open {} using encoding {:?}",
            self.path.display(),
            self.encoding
        );

        let (compressed_bytes, decompressed_bytes) = match stats {
            Some(stats) => (
                Some(&stats.compressed_bytes),
                Some(&stats.decompressed_bytes),
            ),
            None => (None, None),
        };

        let file = async_fs_util::open(&self.path)
            .await
            .categorize_tagged(ErrorTag::EventLogNotFound)?;
        let file: Box<dyn AsyncRead + Send + Sync + Unpin> = match tail {
            Some(tail) => Box::new(TailReader::new(file, tail)),
            None => Box::new(file),
        };
        let file = CountingReader::new(file, compressed_bytes);
        let file = match self.encoding.compression {
            Compression::None => {
                Box::new(CountingReader::new(file, decompressed_bytes)) as EventLogReader
            }
            Compression::Gzip => Box::new(CountingReader::new(
                GzipDecoder::new(BufReader::new(file)),
                decompressed_bytes,
            )) as EventLogReader,
            Compression::Zstd => Box::new(CountingReader::new(
                ZstdDecoder::new(BufReader::new(file)),
                decompressed_bytes,
            )) as EventLogReader,
        };

        Ok(file)
    }

    pub async fn get_summary(&self) -> buck2_error::Result<EventLogSummary> {
        let (invocation, events) = self.unpack_stream().await?;
        let buck_event: BuckEvent = events
            .try_filter_map(|log| {
                let maybe_buck_event = match log {
                    StreamValue::Result(_) | StreamValue::PartialResult(_) => None,
                    StreamValue::Event(buck_event) => Some(buck_event),
                };
                futures::future::ready(Ok(maybe_buck_event))
            })
            .try_next()
            .await?
            .ok_or_else(|| EventLogErrors::EndOfFile(self.path.to_str().unwrap().to_owned()))?
            .try_into()?;
        Ok(EventLogSummary {
            trace_id: buck_event.trace_id()?,
            timestamp: buck_event.timestamp(),
            invocation,
        })
    }

    pub fn extension(&self) -> &str {
        self.encoding.extensions[0]
    }
}

#[cfg(test)]
mod tests {
    use buck2_cli_proto::CommandProgress;
    use buck2_cli_proto::command_progress;
    use buck2_data::CommandStart;
    use buck2_data::SpanStartEvent;
    use buck2_events::span::SpanId;
    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

    use super::*;
    use crate::file_names::get_logfile_name;

    #[test]
    fn test_get_uuid_from_logfile_name() -> buck2_error::Result<()> {
        // Create a test log path.
        let event = buck_event()?;
        let file_name = &get_logfile_name(&event, Encoding::PROTO_ZSTD, "bzl")?;
        let path = EventLogPathBuf {
            path: logdir().as_abs_path().join(file_name),
            encoding: Encoding::PROTO_ZSTD,
            unmarked_log_reading: UnmarkedLogReading::default(),
        };

        // Check we can extract UUID from log path.
        let uuid = path.uuid_from_filename().unwrap();
        assert_eq!(&uuid.to_string(), "7b797fa8-62f1-4123-85f9-875cd74b0a63");

        Ok(())
    }

    fn buck_event() -> Result<BuckEvent, buck2_error::Error> {
        let event = BuckEvent::new(
            SystemTime::now(),
            TraceId::from_str("7b797fa8-62f1-4123-85f9-875cd74b0a63")?,
            Some(SpanId::next()),
            Some(SpanId::next()),
            SpanStartEvent {
                data: Some(
                    CommandStart {
                        ..Default::default()
                    }
                    .into(),
                ),
            }
            .into(),
        );
        Ok(event)
    }

    fn logdir() -> AbsNormPathBuf {
        if cfg!(windows) {
            AbsNormPathBuf::new("C:\\foo".into()).unwrap()
        } else {
            AbsNormPathBuf::new("/foo".into()).unwrap()
        }
    }

    /// A complete `DetailedAggregatedMetrics` event as written before
    /// `AggregatedBuildMetrics.local_test_result_cache_hits` existed: the current event with
    /// exactly that member removed (the interface review's baseline-schema fixture, verbatim).
    const BASELINE_METRICS_EVENT_JSON: &str = r#"{"Event":{"data":{"Instant":{"data":{"DetailedAggregatedMetrics":{"all_targets_build_metrics":{"action_graph_size":null,"compute_time_ms":null,"metrics":{"analysis_retained_memory":0.0,"declared_actions":0.0,"full_graph_execution_time_ms":0.0,"full_graph_output_size_bytes":0.0,"local_execution_time_ms":0.0,"local_executions":0.0,"remote_cache_hits":0.0,"remote_execution_time_ms":0.0,"remote_executions":0.0}},"top_level_target_metrics":[]}}}},"parent_id":0,"span_id":0,"timestamp":null,"trace_id":""}}"#;

    fn metrics_event(local_test_result_cache_hits: f64) -> buck2_data::BuckEvent {
        buck2_data::BuckEvent {
            data: Some(buck2_data::buck_event::Data::Instant(
                buck2_data::InstantEvent {
                    data: Some(buck2_data::instant_event::Data::DetailedAggregatedMetrics(
                        buck2_data::DetailedAggregatedMetrics {
                            all_targets_build_metrics: Some(buck2_data::AllTargetsBuildMetrics {
                                metrics: Some(buck2_data::AggregatedBuildMetrics {
                                    local_executions: 2.0,
                                    local_test_result_cache_hits,
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                },
            )),
            ..Default::default()
        }
    }

    fn aggregated_metrics(value: &StreamValue) -> &buck2_data::AggregatedBuildMetrics {
        let StreamValue::Event(event) = value else {
            panic!("expected an event");
        };
        match event.data.as_ref() {
            Some(buck2_data::buck_event::Data::Instant(buck2_data::InstantEvent {
                data: Some(buck2_data::instant_event::Data::DetailedAggregatedMetrics(m)),
            })) => m
                .all_targets_build_metrics
                .as_ref()
                .and_then(|m| m.metrics.as_ref())
                .expect("aggregated metrics"),
            other => panic!("expected aggregated metrics, got {other:?}"),
        }
    }

    fn invocation_proto() -> buck2_data::Invocation {
        buck2_data::Invocation {
            command_line_args: vec!["buck2".to_owned(), "test".to_owned()],
            expanded_command_line_args: vec!["buck2".to_owned(), "test".to_owned()],
            working_dir: "/work".to_owned(),
            trace_id: Some("7b797fa8-62f1-4123-85f9-875cd74b0a63".to_owned()),
            start_time: None,
            muninn_event_schema: None,
        }
    }

    async fn read_all(
        dir: &tempfile::TempDir,
        name: &str,
        encoding: Encoding,
        bytes: Vec<u8>,
    ) -> buck2_error::Result<Vec<StreamValue>> {
        let path = AbsPathBuf::try_from(dir.path().join(name)).unwrap();
        std::fs::write(&path, bytes)?;
        let log = EventLogPathBuf {
            path,
            encoding,
            unmarked_log_reading: UnmarkedLogReading::default(),
        };
        let (invocation, events) = log.unpack_stream().await?;
        assert_eq!(invocation.command_line_args, ["buck2", "test"]);
        events.try_collect().await
    }

    /// A JSON log line holding an event written before the counter existed, followed by a
    /// current event: both decode through the maintained reader, so the failure cannot be the
    /// last line that the truncation tolerance would hide.
    #[tokio::test]
    async fn test_json_log_with_baseline_schema_metrics_event() -> buck2_error::Result<()> {
        let baseline: serde_json::Value = serde_json::from_str(BASELINE_METRICS_EVENT_JSON)?;
        assert!(
            !BASELINE_METRICS_EVENT_JSON.contains("local_test_result_cache_hits"),
            "the baseline fixture must lack the new member: {baseline}"
        );
        let following = serde_json::to_string(&StreamValue::Event(Box::new(metrics_event(3.0))))?;
        let log = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&invocation_proto())?,
            BASELINE_METRICS_EVENT_JSON,
            following,
        );

        let dir = tempfile::tempdir()?;
        let values = read_all(&dir, "log.json-lines", Encoding::JSON, log.into_bytes()).await?;
        assert_eq!(values.len(), 2);
        let old = aggregated_metrics(&values[0]);
        assert_eq!(old, &buck2_data::AggregatedBuildMetrics::default());
        let current = aggregated_metrics(&values[1]);
        assert_eq!(current.local_test_result_cache_hits, 3.0);
        assert_eq!(current.local_executions, 2.0);
        Ok(())
    }

    /// Current JSON control: the counter round-trips through the writer's JSON and the reader.
    #[tokio::test]
    async fn test_json_log_with_current_schema_metrics_event() -> buck2_error::Result<()> {
        let event = serde_json::to_string(&StreamValue::Event(Box::new(metrics_event(5.0))))?;
        assert!(event.contains("\"local_test_result_cache_hits\":5.0"));
        let log = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&invocation_proto())?,
            event,
            event,
        );
        let dir = tempfile::tempdir()?;
        let values = read_all(&dir, "log.json-lines", Encoding::JSON, log.into_bytes()).await?;
        assert_eq!(values.len(), 2);
        for value in &values {
            assert_eq!(aggregated_metrics(value).local_test_result_cache_hits, 5.0);
        }
        Ok(())
    }

    /// Protobuf control: an absent counter (as in a baseline message) decodes to zero and a set
    /// counter round-trips, through the maintained protobuf reader.
    #[tokio::test]
    async fn test_protobuf_log_with_metrics_events() -> buck2_error::Result<()> {
        let mut bytes = Vec::new();
        invocation_proto()
            .encode_length_delimited(&mut bytes)
            .unwrap();
        for hits in [0.0, 7.0] {
            CommandProgress {
                progress: Some(command_progress::Progress::Event(Box::new(metrics_event(
                    hits,
                )))),
            }
            .encode_length_delimited(&mut bytes)
            .unwrap();
        }
        let dir = tempfile::tempdir()?;
        let values = read_all(&dir, "log.pb", Encoding::PROTO, bytes).await?;
        assert_eq!(values.len(), 2);
        assert_eq!(
            aggregated_metrics(&values[0]).local_test_result_cache_hits,
            0.0
        );
        assert_eq!(
            aggregated_metrics(&values[1]).local_test_result_cache_hits,
            7.0
        );
        Ok(())
    }

    /// A reused `TestResult` event (`buck2_data::TestResult.reuse`).
    fn test_result_event(reuse: buck2_data::TestResultReuse) -> buck2_data::BuckEvent {
        buck2_data::BuckEvent {
            data: Some(buck2_data::buck_event::Data::Instant(
                buck2_data::InstantEvent {
                    data: Some(buck2_data::instant_event::Data::TestResult(
                        buck2_data::TestResult {
                            name: "case".to_owned(),
                            status: buck2_data::TestStatus::Pass as i32,
                            duration: Some(std::time::Duration::from_secs(8).try_into().unwrap()),
                            reuse: Some(Box::new(reuse)),
                            ..Default::default()
                        },
                    )),
                },
            )),
            ..Default::default()
        }
    }

    fn test_result_reuse(value: &StreamValue) -> &buck2_data::TestResultReuse {
        let StreamValue::Event(event) = value else {
            panic!("expected an event");
        };
        match event.data.as_ref() {
            Some(buck2_data::buck_event::Data::Instant(buck2_data::InstantEvent {
                data: Some(buck2_data::instant_event::Data::TestResult(result)),
            })) => result.reuse.as_deref().expect("a reused result"),
            other => panic!("expected a test result, got {other:?}"),
        }
    }

    /// A remote reuse record with a nonempty digest and nonzero original and current times.
    fn remote_reuse(
        served_by: Option<buck2_data::RemoteCommandOrigin>,
    ) -> buck2_data::TestResultReuse {
        buck2_data::TestResultReuse {
            current_execution_time: Some(std::time::Duration::from_millis(250).try_into().unwrap()),
            action_digest: "abc:12".to_owned(),
            served_by: served_by.map(|o| o as i32),
            original_execution_time: Some(std::time::Duration::from_secs(9).try_into().unwrap()),
            ..Default::default()
        }
    }

    /// The `served_by` values a current record round-trips: absent, and each remote answer,
    /// including `ACTION_CACHE (0)`, which presence keeps distinct from absent.
    const SERVED_BY: [Option<buck2_data::RemoteCommandOrigin>; 4] = [
        None,
        Some(buck2_data::RemoteCommandOrigin::ActionCache),
        Some(buck2_data::RemoteCommandOrigin::ReServedFromCache),
        Some(buck2_data::RemoteCommandOrigin::ReDeduplicated),
    ];

    /// A host-store `TestResult` event exactly as the published fork schema (b1100380) writes it
    /// to a JSON log: `TestResultReuse` has only `origin_attempt`, `entry_manifest`,
    /// `current_attempt` and `current_execution_time_us`.
    const B1100380_TEST_RESULT_EVENT_JSON: &str = r#"{"Event":{"data":{"Instant":{"data":{"TestResult":{"details":"","duration_us":8000000,"max_memory_used_bytes":null,"msg":null,"name":"case","reuse":{"current_attempt":"current-attempt","current_execution_time_us":0,"entry_manifest":"entry-manifest","origin_attempt":"origin-attempt"},"status":1,"target_label":null}}}},"parent_id":0,"span_id":0,"timestamp":null,"trace_id":""}}"#;

    /// A b1100380-schema JSON record, followed by another record, decodes through the
    /// maintained reader with no remote provenance: `served_by` absent (never ACTION_CACHE),
    /// no digest, no original time.
    #[tokio::test]
    async fn test_json_log_with_b1100380_schema_test_result_reuse() -> buck2_error::Result<()> {
        for member in ["action_digest", "served_by", "original_execution_time"] {
            assert!(
                !B1100380_TEST_RESULT_EVENT_JSON.contains(member),
                "the b1100380 fixture must lack `{member}`"
            );
        }
        let following = serde_json::to_string(&StreamValue::Event(Box::new(test_result_event(
            remote_reuse(Some(buck2_data::RemoteCommandOrigin::ReServedFromCache)),
        ))))?;
        let log = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&invocation_proto())?,
            B1100380_TEST_RESULT_EVENT_JSON,
            following,
        );
        let dir = tempfile::tempdir()?;
        let values = read_all(&dir, "log.json-lines", Encoding::JSON, log.into_bytes()).await?;
        assert_eq!(values.len(), 2);
        assert_eq!(
            test_result_reuse(&values[0]),
            &buck2_data::TestResultReuse {
                origin_attempt: "origin-attempt".to_owned(),
                entry_manifest: "entry-manifest".to_owned(),
                current_attempt: "current-attempt".to_owned(),
                current_execution_time: Some(std::time::Duration::ZERO.try_into()?),
                action_digest: String::new(),
                served_by: None,
                original_execution_time: None,
            }
        );
        assert_eq!(
            test_result_reuse(&values[1]),
            &remote_reuse(Some(buck2_data::RemoteCommandOrigin::ReServedFromCache))
        );
        Ok(())
    }

    /// Current JSON: each `served_by` (absent, 0, 3, 4) with digest and both times round-trips
    /// through the writer's JSON and the maintained reader.
    #[tokio::test]
    async fn test_json_log_round_trips_served_by() -> buck2_error::Result<()> {
        let mut log = serde_json::to_string(&invocation_proto())? + "\n";
        for served_by in SERVED_BY {
            let line = serde_json::to_string(&StreamValue::Event(Box::new(test_result_event(
                remote_reuse(served_by),
            ))))?;
            assert!(
                line.contains("\"original_execution_time_us\":9000000"),
                "{line}"
            );
            assert!(
                line.contains("\"current_execution_time_us\":250000"),
                "{line}"
            );
            match served_by {
                None => assert!(line.contains("\"served_by\":null"), "{line}"),
                Some(o) => assert!(
                    line.contains(&format!("\"served_by\":{}", o as i32)),
                    "{line}"
                ),
            }
            log += &line;
            log += "\n";
        }
        let dir = tempfile::tempdir()?;
        let values = read_all(&dir, "log.json-lines", Encoding::JSON, log.into_bytes()).await?;
        assert_eq!(values.len(), SERVED_BY.len());
        for (value, served_by) in values.iter().zip(SERVED_BY) {
            assert_eq!(test_result_reuse(value), &remote_reuse(served_by));
        }
        Ok(())
    }

    /// Protobuf: a record without the new fields (as b1100380 writes it), followed by current
    /// records for each `served_by`, through the maintained protobuf reader.
    #[tokio::test]
    async fn test_protobuf_log_round_trips_served_by() -> buck2_error::Result<()> {
        let host_store = buck2_data::TestResultReuse {
            origin_attempt: "origin-attempt".to_owned(),
            entry_manifest: "entry-manifest".to_owned(),
            current_attempt: "current-attempt".to_owned(),
            current_execution_time: Some(std::time::Duration::ZERO.try_into()?),
            ..Default::default()
        };
        let mut expected = vec![host_store];
        expected.extend(SERVED_BY.map(remote_reuse));
        let mut bytes = Vec::new();
        invocation_proto()
            .encode_length_delimited(&mut bytes)
            .unwrap();
        for reuse in &expected {
            CommandProgress {
                progress: Some(command_progress::Progress::Event(Box::new(
                    test_result_event(reuse.clone()),
                ))),
            }
            .encode_length_delimited(&mut bytes)
            .unwrap();
        }
        let dir = tempfile::tempdir()?;
        let values = read_all(&dir, "log.pb", Encoding::PROTO, bytes).await?;
        assert_eq!(values.len(), expected.len());
        for (value, reuse) in values.iter().zip(&expected) {
            assert_eq!(test_result_reuse(value), reuse);
        }
        assert_eq!(test_result_reuse(&values[0]).served_by, None);
        assert_eq!(
            test_result_reuse(&values[2]).served_by,
            Some(buck2_data::RemoteCommandOrigin::ActionCache as i32)
        );
        assert_eq!(test_result_reuse(&values[1]).served_by, None);
        Ok(())
    }

    #[tokio::test]
    async fn protobuf_frames_keep_the_invocation_and_record_order() {
        const TRACE_ID: &str = "7b797fa8-62f1-4123-85f9-875cd74b0a63";
        let span_progress = |span_id| CommandProgress {
            progress: Some(command_progress::Progress::Event(Box::new(
                buck2_data::BuckEvent {
                    trace_id: TRACE_ID.to_owned(),
                    span_id,
                    ..Default::default()
                },
            ))),
        };
        let dir = tempfile::tempdir().unwrap();
        let invocation = buck2_data::Invocation {
            command_line_args: vec!["buck2".to_owned(), "build".to_owned()],
            trace_id: Some(TRACE_ID.to_owned()),
            ..Default::default()
        };
        let records: Vec<CommandProgress> = (1..=3).map(span_progress).collect();
        let mut body = invocation.encode_length_delimited_to_vec();
        for record in &records {
            body.extend(record.encode_length_delimited_to_vec());
        }
        let path = dir.path().join("log.pb");
        tokio::fs::write(&path, body).await.unwrap();
        let log = EventLogPathBuf {
            path: AbsPathBuf::try_from(path).unwrap(),
            encoding: Encoding::PROTO,
            unmarked_log_reading: UnmarkedLogReading::default(),
        };

        assert!(log.is_protobuf());
        let (header, frames) = log.unpack_protobuf_frames().await.unwrap();
        assert_eq!(header.trace_id.to_string(), TRACE_ID);
        let frames: Vec<_> = frames.try_collect().await.unwrap();
        let decoded: Vec<CommandProgress> = frames
            .into_iter()
            .map(|frame| CommandProgress::decode_length_delimited(frame).unwrap())
            .collect();
        assert_eq!(
            decoded, records,
            "the frames are the records after the invocation, in file order"
        );
    }

    #[tokio::test]
    async fn protobuf_frames_fail_for_json_logs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.json-lines");
        tokio::fs::write(&path, b"{}\n").await.unwrap();
        let log = EventLogPathBuf {
            path: AbsPathBuf::try_from(path).unwrap(),
            encoding: Encoding::JSON,
            unmarked_log_reading: UnmarkedLogReading::default(),
        };
        assert!(!log.is_protobuf());
        assert!(
            log.unpack_protobuf_frames().await.is_err(),
            "a JSON log has no protobuf frames"
        );
    }
}

/// The four numbers the fork moved at the 2026-11-01 upstream sync, through the maintained reader
/// (`EventLogPathBuf::unpack_stream`) and the event observer's display: logs the fork wrote at
/// b1100380 (encoded with b1100380's definitions), logs upstream writes (unmarked), and logs
/// current writers mark. See `crate::schema_compat`.
#[cfg(test)]
mod schema_compat_reader_tests {
    use buck2_cli_proto::CommandProgress;
    use buck2_cli_proto::command_progress;
    use buck2_data::ActionExecutionKind;
    use buck2_data::executor_stage_start::Stage;
    use buck2_event_observer::display::CriticalPathEntryDisplay;
    use buck2_event_observer::display::TargetDisplayOptions;
    use buck2_event_observer::display::display_executor_stage;
    use buck2_event_observer::display::display_executor_stage_name;
    use prost::bytes::BytesMut;

    use super::*;
    use crate::schema_compat::LogSchema;
    use crate::schema_compat::MUNINN_EVENT_SCHEMA_CURRENT;
    use crate::schema_compat::b1100380;

    /// The containers from `CommandProgress` down to the renumbered members, as b1100380 numbers
    /// them (the same as now), with only the members these fixtures set; the leaves are
    /// `schema_compat::b1100380`'s exact b1100380 definitions. A proto3 encoder writes only set
    /// members, in field order, so these encode exactly the bytes b1100380's full types write for
    /// the same content.
    mod fixture {
        use super::b1100380;

        #[derive(Clone, PartialEq, prost::Message)]
        pub struct CommandProgress {
            #[prost(message, optional, tag = "1")]
            pub event: Option<BuckEvent>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct BuckEvent {
            #[prost(oneof = "buck_event::Data", tags = "20, 21, 22, 23")]
            pub data: Option<buck_event::Data>,
        }
        pub mod buck_event {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Data {
                #[prost(message, tag = "20")]
                SpanStart(super::SpanStartEvent),
                #[prost(message, tag = "21")]
                SpanEnd(super::SpanEndEvent),
                #[prost(message, tag = "22")]
                Instant(super::InstantEvent),
                #[prost(message, tag = "23")]
                Record(super::RecordEvent),
            }
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SpanStartEvent {
            #[prost(message, optional, tag = "54")]
            pub executor_stage: Option<b1100380::ExecutorStageStart>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SpanEndEvent {
            #[prost(message, optional, tag = "51")]
            pub action_execution: Option<ActionExecutionEnd>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct ActionExecutionEnd {
            #[prost(enumeration = "b1100380::ActionExecutionKind", tag = "22")]
            pub execution_kind: i32,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct InstantEvent {
            #[prost(message, optional, tag = "4")]
            pub build_graph_info: Option<BuildGraphExecutionInfo>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct BuildGraphExecutionInfo {
            #[prost(message, repeated, tag = "5")]
            pub critical_path2: Vec<CriticalPathEntry2>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct CriticalPathEntry2 {
            #[prost(message, optional, tag = "101")]
            pub action_execution: Option<CriticalPathActionExecution>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct CriticalPathActionExecution {
            #[prost(enumeration = "b1100380::ActionExecutionKind", tag = "5")]
            pub execution_kind: i32,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct RecordEvent {
            #[prost(message, optional, tag = "1")]
            pub invocation_record: Option<InvocationRecord>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct InvocationRecord {
            #[prost(uint64, tag = "10")]
            pub run_local_count: u64,
            #[prost(uint64, optional, tag = "761")]
            pub run_local_test_result_cache_count: Option<u64>,
        }

        /// `CommandProgress.event` > `BuckEvent.span_start` > `SpanStartEvent.executor_stage`
        /// holding raw bytes, for a payload no schema accepts.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct RawCommandProgress {
            #[prost(message, optional, tag = "1")]
            pub event: Option<RawBuckEvent>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct RawBuckEvent {
            #[prost(message, optional, tag = "20")]
            pub span_start: Option<RawSpanStartEvent>,
        }
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct RawSpanStartEvent {
            #[prost(bytes = "vec", tag = "54")]
            pub executor_stage: Vec<u8>,
        }
    }

    const TRACE_ID: &str = "7b797fa8-62f1-4123-85f9-875cd74b0a63";

    fn header(muninn_event_schema: Option<u32>) -> buck2_data::Invocation {
        buck2_data::Invocation {
            command_line_args: vec!["buck2".to_owned(), "test".to_owned()],
            expanded_command_line_args: vec!["buck2".to_owned(), "test".to_owned()],
            working_dir: "/work".to_owned(),
            trace_id: Some(TRACE_ID.to_owned()),
            start_time: None,
            muninn_event_schema,
        }
    }

    // ---- b1100380 fixtures ----

    fn legacy_query(digest: &str, attempt: &str) -> b1100380::LocalTestResultCacheQuery {
        b1100380::LocalTestResultCacheQuery {
            action_digest: digest.to_owned(),
            cache_key: "key".to_owned(),
            current_attempt: attempt.to_owned(),
        }
    }

    fn legacy_hit() -> b1100380::LocalTestResultCache {
        b1100380::LocalTestResultCache {
            action_digest: "abc:12".to_owned(),
            cache_key: "key".to_owned(),
            origin_attempt: "origin".to_owned(),
            entry_manifest: "manifest".to_owned(),
            current_attempt: "a3".to_owned(),
        }
    }

    fn legacy_frame(data: fixture::buck_event::Data) -> Vec<u8> {
        fixture::CommandProgress {
            event: Some(fixture::BuckEvent { data: Some(data) }),
        }
        .encode_length_delimited_to_vec()
    }

    fn legacy_stage(stage: b1100380::executor_stage_start::Stage) -> Vec<u8> {
        legacy_frame(fixture::buck_event::Data::SpanStart(
            fixture::SpanStartEvent {
                executor_stage: Some(b1100380::ExecutorStageStart { stage: Some(stage) }),
            },
        ))
    }

    fn legacy_action_end(kind: b1100380::ActionExecutionKind) -> Vec<u8> {
        legacy_frame(fixture::buck_event::Data::SpanEnd(fixture::SpanEndEvent {
            action_execution: Some(fixture::ActionExecutionEnd {
                execution_kind: kind as i32,
            }),
        }))
    }

    /// The frames of a b1100380 log, in this order:
    /// 0. stage 25, a query with a digest;
    /// 1. stage 25, a query with an empty digest;
    /// 2. stage 26, a hit;
    /// 3. an action that ended with kind 12;
    /// 4. a critical path with kinds 12 and 2;
    /// 5. an invocation record with 761 = 5;
    /// 6. an action that ended with kind 1, after them all.
    fn b1100380_frames() -> Vec<Vec<u8>> {
        use b1100380::ActionExecutionKind as K;
        use b1100380::executor_stage_start::Stage as L;
        vec![
            legacy_stage(L::LocalTestResultCacheQuery(legacy_query("abc:12", "a1"))),
            legacy_stage(L::LocalTestResultCacheQuery(legacy_query("", "a2"))),
            legacy_stage(L::LocalTestResultCache(legacy_hit())),
            legacy_action_end(K::LocalTestResultCache),
            legacy_frame(fixture::buck_event::Data::Instant(fixture::InstantEvent {
                build_graph_info: Some(fixture::BuildGraphExecutionInfo {
                    critical_path2: [K::LocalTestResultCache, K::Remote]
                        .into_iter()
                        .map(|k| fixture::CriticalPathEntry2 {
                            action_execution: Some(fixture::CriticalPathActionExecution {
                                execution_kind: k as i32,
                            }),
                        })
                        .collect(),
                }),
            })),
            legacy_frame(fixture::buck_event::Data::Record(fixture::RecordEvent {
                invocation_record: Some(fixture::InvocationRecord {
                    run_local_count: 3,
                    run_local_test_result_cache_count: Some(5),
                }),
            })),
            legacy_action_end(K::Local),
        ]
    }

    fn protobuf_log(header: buck2_data::Invocation, frames: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = header.encode_length_delimited_to_vec();
        for frame in frames {
            bytes.extend_from_slice(frame);
        }
        bytes
    }

    async fn read(
        name: &str,
        encoding: Encoding,
        bytes: Vec<u8>,
        reading: UnmarkedLogReading,
    ) -> buck2_error::Result<(Invocation, Vec<StreamValue>)> {
        let dir = tempfile::tempdir()?;
        let path = AbsPathBuf::try_from(dir.path().join(name)).unwrap();
        std::fs::write(&path, bytes)?;
        let log = EventLogPathBuf {
            path,
            encoding,
            unmarked_log_reading: UnmarkedLogReading::default(),
        }
        .with_unmarked_log_reading(reading);
        let (invocation, events) = log.unpack_stream().await?;
        Ok((invocation, events.try_collect().await?))
    }

    // ---- accessors ----

    fn event(value: &StreamValue) -> &buck2_data::BuckEvent {
        match value {
            StreamValue::Event(e) => e,
            _ => panic!("expected an event"),
        }
    }

    fn stage(value: &StreamValue) -> &Stage {
        match event(value).data.as_ref() {
            Some(buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::ExecutorStage(s)),
            })) => s.stage.as_ref().expect("a stage"),
            other => panic!("expected an executor stage, got {other:?}"),
        }
    }

    fn stage_label(value: &StreamValue) -> &'static str {
        display_executor_stage(stage(value)).expect("a label")
    }

    fn action_kind(value: &StreamValue) -> i32 {
        match event(value).data.as_ref() {
            Some(buck2_data::buck_event::Data::SpanEnd(end)) => match end.data.as_ref() {
                Some(buck2_data::span_end_event::Data::ActionExecution(a)) => a.execution_kind,
                other => panic!("expected an action execution end, got {other:?}"),
            },
            other => panic!("expected a span end, got {other:?}"),
        }
    }

    /// Each critical path entry's kind, and how `buck2 log critical-path` shows it.
    fn critical_path_kinds(value: &StreamValue) -> Vec<(i32, &'static str)> {
        match event(value).data.as_ref() {
            Some(buck2_data::buck_event::Data::Instant(buck2_data::InstantEvent {
                data: Some(buck2_data::instant_event::Data::BuildGraphInfo(info)),
            })) => info
                .critical_path2
                .iter()
                .map(|entry| {
                    let Some(buck2_data::critical_path_entry2::Entry::ActionExecution(a)) =
                        &entry.entry
                    else {
                        panic!("expected an action entry")
                    };
                    let shown = CriticalPathEntryDisplay::from_entry(
                        entry,
                        TargetDisplayOptions::for_log(),
                    )
                    .unwrap()
                    .unwrap()
                    .execution_kind
                    .unwrap();
                    (a.execution_kind, shown)
                })
                .collect(),
            other => panic!("expected build graph info, got {other:?}"),
        }
    }

    fn record(value: &StreamValue) -> &buck2_data::InvocationRecord {
        match event(value).data.as_ref() {
            Some(buck2_data::buck_event::Data::Record(buck2_data::RecordEvent {
                data: Some(buck2_data::record_event::Data::InvocationRecord(r)),
            })) => r,
            other => panic!("expected an invocation record, got {other:?}"),
        }
    }

    fn query(digest: &str, attempt: &str) -> buck2_data::LocalTestResultCacheQuery {
        legacy_query(digest, attempt).into()
    }

    const AMBIGUOUS: i32 = ActionExecutionKind::ServedFromCacheOrLegacyLocalTestResultCache as i32;
    const AMBIGUOUS_NAME: &str =
        "ACTION_EXECUTION_KIND_SERVED_FROM_CACHE_OR_LEGACY_LOCAL_TEST_RESULT_CACHE";

    /// What the plain current decoder makes of the b1100380 frames, which the reader corrects.
    #[test]
    fn the_current_decoder_alone_fails_or_misreads_b1100380_frames() {
        let frames = b1100380_frames();
        let decode = |i: usize| {
            buck2_event_log_format::decode::protobuf_frame(BytesMut::from(&frames[i][..]))
        };
        // 25 with a digest and 761: the whole record fails.
        assert!(decode(0).is_err());
        assert!(decode(5).is_err());
        // 25 empty: read as cas_artifact. 26: dropped. 12: served from cache.
        assert_eq!(
            stage(&decode(1).unwrap()),
            &Stage::CasArtifact(Default::default())
        );
        match event(&decode(2).unwrap()).data.as_ref() {
            Some(buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::ExecutorStage(s)),
            })) => assert_eq!(s.stage, None),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            action_kind(&decode(3).unwrap()),
            ActionExecutionKind::RemoteServedFromCache as i32
        );
    }

    /// Default reading of an unmarked log the fork wrote at b1100380: what the bytes decide is
    /// recovered (25 with a digest, 761), what they do not is shown as ambiguous (25 empty, 26,
    /// kind 12), and nothing is a decode failure.
    #[tokio::test]
    async fn b1100380_protobuf_log_reads_without_failure_or_mislabel() -> buck2_error::Result<()> {
        let (invocation, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(header(None), &b1100380_frames()),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(invocation.schema(), LogSchema::Unmarked);
        assert_eq!(values.len(), 7);

        assert_eq!(
            stage(&values[0]),
            &Stage::LocalTestResultCacheQuery(query("abc:12", "a1"))
        );
        assert_eq!(stage_label(&values[0]), "local_test_result_cache_query");

        assert_eq!(
            stage(&values[1]),
            &Stage::UnmarkedLogStage(Box::new(buck2_data::UnmarkedLogStage {
                field_number: 25,
                as_cas_artifact: Some(Default::default()),
                as_legacy_local_test_result_cache_query: Some(query("", "a2")),
                as_legacy_local_test_result_cache: None,
                as_current_schema_stage: None,
            }))
        );
        assert_eq!(
            stage_label(&values[1]),
            "cas_artifact or legacy local_test_result_cache_query (log predates schema marker)"
        );

        assert_eq!(
            stage(&values[2]),
            &Stage::UnmarkedLogStage(Box::new(buck2_data::UnmarkedLogStage {
                field_number: 26,
                as_cas_artifact: None,
                as_legacy_local_test_result_cache_query: None,
                as_legacy_local_test_result_cache: Some(legacy_hit().into()),
                as_current_schema_stage: None,
            }))
        );
        assert_eq!(
            stage_label(&values[2]),
            "unknown stage 26 or legacy local_test_result_cache (log predates schema marker)"
        );

        assert_eq!(action_kind(&values[3]), AMBIGUOUS);
        assert_eq!(
            critical_path_kinds(&values[4]),
            [
                (AMBIGUOUS, AMBIGUOUS_NAME),
                (2, "ACTION_EXECUTION_KIND_REMOTE")
            ]
        );

        let r = record(&values[5]);
        assert_eq!(r.run_local_test_result_cache_count, Some(5));
        assert_eq!(r.run_local_count, 3);
        assert!(r.re_exceeded_quotas.is_empty());

        assert_eq!(action_kind(&values[6]), ActionExecutionKind::Local as i32);
        Ok(())
    }

    /// The explicit legacy opt-in gives the b1100380 meaning of every frame.
    #[tokio::test]
    async fn b1100380_protobuf_log_with_legacy_opt_in_reads_the_old_meaning()
    -> buck2_error::Result<()> {
        let (_, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(header(None), &b1100380_frames()),
            UnmarkedLogReading::LegacyFork,
        )
        .await?;
        assert_eq!(values.len(), 7);
        assert_eq!(
            stage(&values[0]),
            &Stage::LocalTestResultCacheQuery(query("abc:12", "a1"))
        );
        assert_eq!(
            stage(&values[1]),
            &Stage::LocalTestResultCacheQuery(query("", "a2"))
        );
        assert_eq!(stage_label(&values[1]), "local_test_result_cache_query");
        assert_eq!(
            stage(&values[2]),
            &Stage::LocalTestResultCache(legacy_hit().into())
        );
        assert_eq!(stage_label(&values[2]), "local_test_result_cache");
        let local_reuse = ActionExecutionKind::LocalTestResultCache as i32;
        assert_eq!(action_kind(&values[3]), local_reuse);
        assert_eq!(
            critical_path_kinds(&values[4]),
            [
                (local_reuse, "ACTION_EXECUTION_KIND_LOCAL_TEST_RESULT_CACHE"),
                (2, "ACTION_EXECUTION_KIND_REMOTE")
            ]
        );
        assert_eq!(
            record(&values[5]).run_local_test_result_cache_count,
            Some(5)
        );
        assert_eq!(action_kind(&values[6]), ActionExecutionKind::Local as i32);
        Ok(())
    }

    fn current_frame(data: buck2_data::buck_event::Data) -> Vec<u8> {
        CommandProgress {
            progress: Some(command_progress::Progress::Event(Box::new(
                buck2_data::BuckEvent {
                    trace_id: TRACE_ID.to_owned(),
                    data: Some(data),
                    ..Default::default()
                },
            ))),
        }
        .encode_length_delimited_to_vec()
    }

    fn current_stage(stage: Stage) -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
            data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                buck2_data::ExecutorStageStart { stage: Some(stage) },
            )),
        })
    }

    fn current_action_end(kind: ActionExecutionKind) -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::SpanEnd(buck2_data::SpanEndEvent {
            data: Some(buck2_data::span_end_event::Data::ActionExecution(Box::new(
                buck2_data::ActionExecutionEnd {
                    execution_kind: kind as i32,
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
    }

    fn current_critical_path(kinds: &[ActionExecutionKind]) -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::Instant(buck2_data::InstantEvent {
            data: Some(buck2_data::instant_event::Data::BuildGraphInfo(
                buck2_data::BuildGraphExecutionInfo {
                    critical_path2: kinds
                        .iter()
                        .map(|k| buck2_data::CriticalPathEntry2 {
                            entry: Some(buck2_data::critical_path_entry2::Entry::ActionExecution(
                                buck2_data::critical_path_entry2::ActionExecution {
                                    execution_kind: *k as i32,
                                    ..Default::default()
                                },
                            )),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
            )),
        })
    }

    fn current_record(record: buck2_data::InvocationRecord) -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::Record(buck2_data::RecordEvent {
            data: Some(buck2_data::record_event::Data::InvocationRecord(Box::new(
                record,
            ))),
        })
    }

    fn cas(phase: buck2_data::cas_artifact_stage::Phase) -> Stage {
        Stage::CasArtifact(buck2_data::CasArtifactStage {
            phase: phase as i32,
        })
    }

    /// The events upstream writes (and current writers, for upstream's members), in this order:
    /// 0. an action that was served from the RE cache (12);
    /// 1. a critical path with 12;
    /// 2. a cas_artifact stage with a phase;
    /// 3. a cas_artifact stage without one (an empty payload);
    /// 4. an invocation record with exceeded quotas at 761;
    /// 5. a deduplicated action (13).
    fn upstream_frames() -> Vec<Vec<u8>> {
        use buck2_data::cas_artifact_stage::Phase;
        vec![
            current_frame(current_action_end(
                ActionExecutionKind::RemoteServedFromCache,
            )),
            current_frame(current_critical_path(&[
                ActionExecutionKind::RemoteServedFromCache,
            ])),
            current_frame(current_stage(cas(Phase::CheckExpiration))),
            current_frame(current_stage(cas(Phase::Unspecified))),
            current_frame(current_record(buck2_data::InvocationRecord {
                re_exceeded_quotas: vec!["quota-a".to_owned()],
                run_remote_served_from_cache_count: 1,
                ..Default::default()
            })),
            current_frame(current_action_end(ActionExecutionKind::RemoteDeduplicated)),
        ]
    }

    /// An unmarked log as upstream writes it. Upstream's own record and phase are decided by their
    /// bytes, in either reading. A 12, or an empty cas_artifact, cannot be told from the fork's
    /// pre-sync values in an unmarked log, so by default it is shown as ambiguous, never as the
    /// fork's meaning.
    #[tokio::test]
    async fn upstream_unmarked_protobuf_log_is_decided_by_bytes_or_ambiguous()
    -> buck2_error::Result<()> {
        for reading in [
            UnmarkedLogReading::Ambiguous,
            UnmarkedLogReading::LegacyFork,
        ] {
            let (invocation, values) = read(
                "log.pb",
                Encoding::PROTO,
                protobuf_log(header(None), &upstream_frames()),
                reading,
            )
            .await?;
            assert_eq!(invocation.schema(), LogSchema::Unmarked);
            assert_eq!(values.len(), 6);
            assert_eq!(
                stage(&values[2]),
                &cas(buck2_data::cas_artifact_stage::Phase::CheckExpiration),
                "{reading:?}"
            );
            assert_eq!(stage_label(&values[2]), "cas_check_expiration");
            let r = record(&values[4]);
            assert_eq!(r.re_exceeded_quotas, ["quota-a"], "{reading:?}");
            assert_eq!(r.run_remote_served_from_cache_count, 1);
            assert_eq!(r.run_local_test_result_cache_count, None);
            assert_eq!(
                action_kind(&values[5]),
                ActionExecutionKind::RemoteDeduplicated as i32
            );
        }

        let (_, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(header(None), &upstream_frames()),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(action_kind(&values[0]), AMBIGUOUS);
        assert_eq!(
            critical_path_kinds(&values[1]),
            [(AMBIGUOUS, AMBIGUOUS_NAME)]
        );
        assert_eq!(
            stage(&values[3]),
            &Stage::UnmarkedLogStage(Box::new(buck2_data::UnmarkedLogStage {
                field_number: 25,
                as_cas_artifact: Some(Default::default()),
                as_legacy_local_test_result_cache_query: Some(Default::default()),
                as_legacy_local_test_result_cache: None,
                as_current_schema_stage: None,
            }))
        );
        Ok(())
    }

    /// The events current writers write for the fork's members, at their current numbers.
    fn current_fork_frames() -> Vec<Vec<u8>> {
        vec![
            current_frame(current_action_end(
                ActionExecutionKind::LocalTestResultCache,
            )),
            current_frame(current_stage(Stage::LocalTestResultCacheQuery(query(
                "", "a2",
            )))),
            current_frame(current_stage(Stage::LocalTestResultCache(
                legacy_hit().into(),
            ))),
            current_frame(current_record(buck2_data::InvocationRecord {
                run_local_test_result_cache_count: Some(5),
                re_exceeded_quotas: vec!["quota-a".to_owned()],
                ..Default::default()
            })),
        ]
    }

    /// A marked log reads with exactly the current meaning, whatever the unmarked-log reading.
    #[tokio::test]
    async fn marked_protobuf_log_reads_the_current_meaning() -> buck2_error::Result<()> {
        let mut frames = upstream_frames();
        frames.extend(current_fork_frames());
        for reading in [
            UnmarkedLogReading::Ambiguous,
            UnmarkedLogReading::LegacyFork,
        ] {
            let (invocation, values) = read(
                "log.pb",
                Encoding::PROTO,
                protobuf_log(header(Some(MUNINN_EVENT_SCHEMA_CURRENT)), &frames),
                reading,
            )
            .await?;
            assert_eq!(invocation.schema(), LogSchema::Marked(2));
            assert_eq!(values.len(), 10);
            assert_eq!(
                action_kind(&values[0]),
                ActionExecutionKind::RemoteServedFromCache as i32
            );
            assert_eq!(
                critical_path_kinds(&values[1]),
                [(12, "ACTION_EXECUTION_KIND_REMOTE_SERVED_FROM_CACHE")]
            );
            assert_eq!(
                stage(&values[3]),
                &cas(buck2_data::cas_artifact_stage::Phase::Unspecified)
            );
            assert_eq!(stage_label(&values[3]), "cas_artifact");
            assert_eq!(
                action_kind(&values[6]),
                ActionExecutionKind::LocalTestResultCache as i32
            );
            assert_eq!(
                stage(&values[7]),
                &Stage::LocalTestResultCacheQuery(query("", "a2"))
            );
            assert_eq!(
                stage(&values[8]),
                &Stage::LocalTestResultCache(legacy_hit().into())
            );
            let r = record(&values[9]);
            assert_eq!(r.run_local_test_result_cache_count, Some(5));
            assert_eq!(r.re_exceeded_quotas, ["quota-a"]);
        }
        Ok(())
    }

    /// A record neither schema accepts keeps the original error: here a 25 whose field 1 is a
    /// fixed64 (neither b1100380's string nor upstream's enum).
    #[tokio::test]
    async fn a_record_neither_schema_reads_surfaces_the_original_error() -> buck2_error::Result<()>
    {
        let mut payload = Vec::new();
        prost::encoding::encode_key(25, prost::encoding::WireType::LengthDelimited, &mut payload);
        prost::encoding::encode_varint(9, &mut payload);
        prost::encoding::encode_key(1, prost::encoding::WireType::SixtyFourBit, &mut payload);
        payload.extend_from_slice(&[0; 8]);
        let frame = fixture::RawCommandProgress {
            event: Some(fixture::RawBuckEvent {
                span_start: Some(fixture::RawSpanStartEvent {
                    executor_stage: payload,
                }),
            }),
        }
        .encode_length_delimited_to_vec();
        let original = buck2_event_log_format::decode::protobuf_frame(BytesMut::from(&frame[..]))
            .err()
            .expect("the current schema rejects it");
        for reading in [
            UnmarkedLogReading::Ambiguous,
            UnmarkedLogReading::LegacyFork,
        ] {
            let err = read(
                "log.pb",
                Encoding::PROTO,
                protobuf_log(header(None), std::slice::from_ref(&frame)),
                reading,
            )
            .await
            .err()
            .expect("an error");
            assert_eq!(err.to_string(), original.to_string(), "{reading:?}");
        }
        Ok(())
    }

    // ---- several stage members on the wire ----

    /// One length-delimited field.
    fn wire_field(number: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        prost::encoding::encode_key(number, prost::encoding::WireType::LengthDelimited, &mut out);
        prost::encoding::encode_varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }

    /// A frame whose `ExecutorStageStart` is exactly `payload`.
    fn raw_stage_frame(payload: Vec<u8>) -> Vec<u8> {
        fixture::RawCommandProgress {
            event: Some(fixture::RawBuckEvent {
                span_start: Some(fixture::RawSpanStartEvent {
                    executor_stage: payload,
                }),
            }),
        }
        .encode_length_delimited_to_vec()
    }

    /// What one reading must make of a stage message.
    enum Expect {
        Stage(Stage),
        /// An `ExecutorStageStart` without a stage.
        NoStage,
        Error,
    }

    /// Reads `[action 1, stage payload, action 2]` through the real reader, unmarked in both
    /// readings and marked, and checks the stage against the expectation for each, and that the
    /// surrounding events still read.
    async fn check_stage_payload(
        case: &str,
        payload: Vec<u8>,
        ambiguous: Expect,
        legacy: Expect,
        marked: Expect,
    ) -> buck2_error::Result<()> {
        check_stage_frame(case, raw_stage_frame(payload), ambiguous, legacy, marked).await
    }

    /// Reads `stage_frame` between two other records, in each reading mode, and checks its stage
    /// and that the records around it are read too.
    async fn check_stage_frame(
        case: &str,
        stage_frame: Vec<u8>,
        ambiguous: Expect,
        legacy: Expect,
        marked: Expect,
    ) -> buck2_error::Result<()> {
        let frames = [
            current_frame(current_action_end(ActionExecutionKind::Local)),
            stage_frame,
            current_frame(current_action_end(ActionExecutionKind::Remote)),
        ];
        for (marker, reading, expect) in [
            (None, UnmarkedLogReading::Ambiguous, &ambiguous),
            (None, UnmarkedLogReading::LegacyFork, &legacy),
            (
                Some(MUNINN_EVENT_SCHEMA_CURRENT),
                UnmarkedLogReading::LegacyFork,
                &marked,
            ),
        ] {
            let what = format!("{case}, marker {marker:?}, {reading:?}");
            let result = read(
                "log.pb",
                Encoding::PROTO,
                protobuf_log(header(marker), &frames),
                reading,
            )
            .await;
            match expect {
                Expect::Stage(want) => {
                    let values = result.unwrap_or_else(|e| panic!("{what}: {e:#}"));
                    assert_eq!(values.1.len(), 3, "{what}");
                    assert_eq!(stage(&values.1[1]), want, "{what}");
                    assert_eq!(
                        action_kind(&values.1[0]),
                        ActionExecutionKind::Local as i32,
                        "{what}"
                    );
                    assert_eq!(
                        action_kind(&values.1[2]),
                        ActionExecutionKind::Remote as i32,
                        "{what}"
                    );
                }
                Expect::NoStage => {
                    let values = result.unwrap_or_else(|e| panic!("{what}: {e:#}"));
                    match event(&values.1[1]).data.as_ref() {
                        Some(buck2_data::buck_event::Data::SpanStart(
                            buck2_data::SpanStartEvent {
                                data: Some(buck2_data::span_start_event::Data::ExecutorStage(s)),
                            },
                        )) => assert_eq!(s.stage, None, "{what}"),
                        other => panic!("{what}: expected an executor stage, got {other:?}"),
                    }
                }
                Expect::Error => assert!(result.is_err(), "{what}: expected an error"),
            }
        }
        Ok(())
    }

    fn prepare() -> Stage {
        Stage::Prepare(Default::default())
    }

    fn unmarked_stage(stage: buck2_data::UnmarkedLogStage) -> Stage {
        Stage::UnmarkedLogStage(Box::new(stage))
    }

    /// R5-WIRE-01. A oneof member is last-wins on the wire: `d2 01 00 c2 01 00` is an empty 26
    /// then an empty 24, and both schemas select 24 (`Prepare`). The 26 before it must not
    /// replace that with an `UnmarkedLogStage`.
    #[tokio::test]
    async fn a_known_stage_after_a_26_is_the_selected_stage() -> buck2_error::Result<()> {
        let payload = [wire_field(26, &[]), wire_field(24, &[])].concat();
        assert_eq!(payload, [0xd2, 0x01, 0x00, 0xc2, 0x01, 0x00]);
        check_stage_payload(
            "26 then 24",
            payload,
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
        )
        .await?;
        // Also when the 26 is not b1100380's hit (field 1 a varint, not a string).
        check_stage_payload(
            "26 (no schema's) then 24",
            [wire_field(26, &[0x08, 0x01]), wire_field(24, &[])].concat(),
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
        )
        .await?;
        // And the display shows it as what it is.
        let (_, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(
                header(None),
                &[raw_stage_frame(
                    [wire_field(26, &[]), wire_field(24, &[])].concat(),
                )],
            ),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(stage_label(&values[0]), "prepare");
        Ok(())
    }

    /// The reverse order: b1100380 selects the 26 (its local test-result cache hit), while the
    /// current schema skips the 26 and selects the 24 (`Prepare`). By default both readings are
    /// kept, and shown; the opt-in reads the hit; a marked log reads `Prepare`.
    #[tokio::test]
    async fn a_26_after_a_known_stage_keeps_both_readings() -> buck2_error::Result<()> {
        let as_current = Some(Box::new(buck2_data::ExecutorStageStart {
            stage: Some(prepare()),
        }));
        let both = unmarked_stage(buck2_data::UnmarkedLogStage {
            field_number: 26,
            as_legacy_local_test_result_cache: Some(Default::default()),
            as_current_schema_stage: as_current.clone(),
            ..Default::default()
        });
        let payload = [wire_field(24, &[]), wire_field(26, &[])].concat();
        assert_eq!(payload, [0xc2, 0x01, 0x00, 0xd2, 0x01, 0x00]);
        check_stage_payload(
            "24 then 26",
            payload.clone(),
            Expect::Stage(both.clone()),
            Expect::Stage(Stage::LocalTestResultCache(Default::default())),
            Expect::Stage(prepare()),
        )
        .await?;
        let (_, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(header(None), &[raw_stage_frame(payload)]),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(stage(&values[0]), &both);
        assert_eq!(
            display_executor_stage_name(stage(&values[0])).as_deref(),
            Some("prepare or legacy local_test_result_cache (log predates schema marker)")
        );
        assert_eq!(
            stage_label(&values[0]),
            "a current-schema stage or legacy local_test_result_cache (log predates schema marker)"
        );

        // A selected 26 that neither schema reads is recorded without a reading of its own, with
        // what the current schema reads, in either mode.
        let neither = unmarked_stage(buck2_data::UnmarkedLogStage {
            field_number: 26,
            as_current_schema_stage: as_current,
            ..Default::default()
        });
        let payload = [wire_field(24, &[]), wire_field(26, &[0x08, 0x01])].concat();
        check_stage_payload(
            "24 then 26 (no schema's)",
            payload.clone(),
            Expect::Stage(neither.clone()),
            Expect::Stage(neither),
            Expect::Stage(prepare()),
        )
        .await?;
        let (_, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(header(None), &[raw_stage_frame(payload)]),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(
            display_executor_stage_name(stage(&values[0])).as_deref(),
            Some("prepare or unknown stage 26, not represented (log predates schema marker)")
        );

        // A 26 alone: the current schema reads no stage, so there is no current reading.
        check_stage_payload(
            "26 alone",
            wire_field(26, &[]),
            Expect::Stage(unmarked_stage(buck2_data::UnmarkedLogStage {
                field_number: 26,
                as_legacy_local_test_result_cache: Some(Default::default()),
                ..Default::default()
            })),
            Expect::Stage(Stage::LocalTestResultCache(Default::default())),
            Expect::NoStage,
        )
        .await
    }

    /// A `CommandProgress` frame whose executor stage is the stage messages `parts`, each in an
    /// occurrence of its own of `SpanStartEvent.executor_stage` (54), with `between` (whole
    /// encoded fields of `SpanStartEvent`) after each occurrence but the last.
    fn split_stage_frame(parts: &[&[u8]], between: &[u8]) -> Vec<u8> {
        let mut span_start = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                span_start.extend_from_slice(between);
            }
            span_start.extend_from_slice(&wire_field(54, part));
        }
        let progress = wire_field(1, &wire_field(20, &span_start));
        let mut frame = Vec::new();
        prost::encoding::encode_varint(progress.len() as u64, &mut frame);
        frame.extend_from_slice(&progress);
        frame
    }

    /// R5-WIRE-02. Protobuf merges repeated occurrences of a message field, so a stage message
    /// split across two occurrences of its enclosing field 54 is the same message as the one
    /// with both parts in a single occurrence, and must be read the same: 24 in one occurrence
    /// and 26 in the next is 24 then 26, with both readings kept by default. Likewise for every
    /// field on the path to it, and for each order and kind of 26.
    #[tokio::test]
    async fn a_stage_split_across_occurrences_reads_as_the_whole_message() -> buck2_error::Result<()>
    {
        let prepare_part = wire_field(24, &[]);
        let hit_part = wire_field(26, &[]);
        let invalid_part = wire_field(26, &[0x08, 0x01]);
        // The reviewer's exact SpanStartEvent payload: `b2 03 03 c2 01 00 b2 03 03 d2 01 00`.
        assert_eq!(
            [wire_field(54, &prepare_part), wire_field(54, &hit_part)].concat(),
            [
                0xb2, 0x03, 0x03, 0xc2, 0x01, 0x00, 0xb2, 0x03, 0x03, 0xd2, 0x01, 0x00
            ]
        );
        for (case, first, second) in [
            ("24 then 26", &prepare_part, &hit_part),
            ("26 then 24", &hit_part, &prepare_part),
            ("24 then 26 (no schema's)", &prepare_part, &invalid_part),
            ("26 (no schema's) then 24", &invalid_part, &prepare_part),
        ] {
            let whole = raw_stage_frame([first.clone(), second.clone()].concat());
            let split = split_stage_frame(&[first, second], &[]);
            // The same message split at each field of its path: `CommandProgress.event`,
            // `BuckEvent.span_start`, `SpanStartEvent.executor_stage`.
            let split_event = {
                let a = wire_field(20, &wire_field(54, first));
                let b = wire_field(20, &wire_field(54, second));
                let progress = [wire_field(1, &a), wire_field(1, &b)].concat();
                let mut frame = Vec::new();
                prost::encoding::encode_varint(progress.len() as u64, &mut frame);
                frame.extend_from_slice(&progress);
                frame
            };
            let split_span = {
                let progress = wire_field(
                    1,
                    &[
                        wire_field(20, &wire_field(54, first)),
                        wire_field(20, &wire_field(54, second)),
                    ]
                    .concat(),
                );
                let mut frame = Vec::new();
                prost::encoding::encode_varint(progress.len() as u64, &mut frame);
                frame.extend_from_slice(&progress);
                frame
            };
            for (marker, reading) in [
                (None, UnmarkedLogReading::Ambiguous),
                (None, UnmarkedLogReading::LegacyFork),
                (
                    Some(MUNINN_EVENT_SCHEMA_CURRENT),
                    UnmarkedLogReading::Ambiguous,
                ),
            ] {
                let read_one = |frame: Vec<u8>| async move {
                    let frames = [
                        current_frame(current_action_end(ActionExecutionKind::Local)),
                        frame,
                        current_frame(current_action_end(ActionExecutionKind::Remote)),
                    ];
                    let (_, values) = read(
                        "log.pb",
                        Encoding::PROTO,
                        protobuf_log(header(marker), &frames),
                        reading,
                    )
                    .await?;
                    assert_eq!(values.len(), 3);
                    assert_eq!(action_kind(&values[0]), ActionExecutionKind::Local as i32);
                    assert_eq!(action_kind(&values[2]), ActionExecutionKind::Remote as i32);
                    Ok::<_, buck2_error::Error>(values)
                };
                let want = read_one(whole.clone()).await?;
                for (how, frame) in [
                    ("split at executor_stage", &split),
                    ("split at span_start", &split_span),
                    ("split at event", &split_event),
                ] {
                    let got = read_one(frame.clone()).await?;
                    let what = format!("{case}, {how}, marker {marker:?}, {reading:?}");
                    assert_eq!(stage(&got[1]), stage(&want[1]), "{what}");
                    assert_eq!(
                        display_executor_stage_name(stage(&got[1])),
                        display_executor_stage_name(stage(&want[1])),
                        "{what}"
                    );
                    assert_eq!(stage_label(&got[1]), stage_label(&want[1]), "{what}");
                }
            }
        }

        // The reviewer's case, with what it must read as, not only that the two encodings agree.
        check_stage_frame(
            "24 then 26, split at executor_stage",
            split_stage_frame(&[&prepare_part, &hit_part], &[]),
            Expect::Stage(unmarked_stage(buck2_data::UnmarkedLogStage {
                field_number: 26,
                as_legacy_local_test_result_cache: Some(Default::default()),
                as_current_schema_stage: Some(Box::new(buck2_data::ExecutorStageStart {
                    stage: Some(prepare()),
                })),
                ..Default::default()
            })),
            Expect::Stage(Stage::LocalTestResultCache(Default::default())),
            Expect::Stage(prepare()),
        )
        .await?;
        let (_, values) = read(
            "log.pb",
            Encoding::PROTO,
            protobuf_log(
                header(None),
                &[split_stage_frame(&[&prepare_part, &hit_part], &[])],
            ),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(
            display_executor_stage_name(stage(&values[0])).as_deref(),
            Some("prepare or legacy local_test_result_cache (log predates schema marker)")
        );

        // Another member of `SpanStartEvent.data` between the occurrences (`command = 50`) ends
        // the first: protobuf then reads the second occurrence as a new message, a 26 alone.
        check_stage_frame(
            "24, another member, then 26",
            split_stage_frame(&[&prepare_part, &hit_part], &wire_field(50, &[])),
            Expect::Stage(unmarked_stage(buck2_data::UnmarkedLogStage {
                field_number: 26,
                as_legacy_local_test_result_cache: Some(Default::default()),
                ..Default::default()
            })),
            Expect::Stage(Stage::LocalTestResultCache(Default::default())),
            Expect::NoStage,
        )
        .await?;
        // A field that is not a member of that oneof (here unknown to both schemas) does not.
        check_stage_frame(
            "24, an unknown field, then 26",
            split_stage_frame(&[&prepare_part, &hit_part], &wire_field(9999, &[])),
            Expect::Stage(unmarked_stage(buck2_data::UnmarkedLogStage {
                field_number: 26,
                as_legacy_local_test_result_cache: Some(Default::default()),
                as_current_schema_stage: Some(Box::new(buck2_data::ExecutorStageStart {
                    stage: Some(prepare()),
                })),
                ..Default::default()
            })),
            Expect::Stage(Stage::LocalTestResultCache(Default::default())),
            Expect::Stage(prepare()),
        )
        .await
    }

    /// The same for 25: a known stage after it is selected; a 25 after a known stage follows the
    /// existing 25 rules.
    #[tokio::test]
    async fn stage_25_rules_apply_only_when_25_is_selected() -> buck2_error::Result<()> {
        let digest_query = b1100380::LocalTestResultCacheQuery {
            action_digest: "abc:12".to_owned(),
            ..Default::default()
        }
        .encode_to_vec();
        check_stage_payload(
            "25 (both schemas') then 24",
            [wire_field(25, &[]), wire_field(24, &[])].concat(),
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
        )
        .await?;
        // Only b1100380 reads this message (its 25 has a digest), and it selects 24. The
        // current schema rejects the whole message, so the marked log fails as before.
        check_stage_payload(
            "25 (b1100380's) then 24",
            [wire_field(25, &digest_query), wire_field(24, &[])].concat(),
            Expect::Stage(prepare()),
            Expect::Stage(prepare()),
            Expect::Error,
        )
        .await?;
        check_stage_payload(
            "24 then 25 (both schemas')",
            [wire_field(24, &[]), wire_field(25, &[])].concat(),
            Expect::Stage(unmarked_stage(buck2_data::UnmarkedLogStage {
                field_number: 25,
                as_cas_artifact: Some(Default::default()),
                as_legacy_local_test_result_cache_query: Some(Default::default()),
                ..Default::default()
            })),
            Expect::Stage(Stage::LocalTestResultCacheQuery(Default::default())),
            Expect::Stage(cas(buck2_data::cas_artifact_stage::Phase::Unspecified)),
        )
        .await?;
        check_stage_payload(
            "24 then 25 (b1100380's)",
            [wire_field(24, &[]), wire_field(25, &digest_query)].concat(),
            Expect::Stage(Stage::LocalTestResultCacheQuery(
                buck2_data::LocalTestResultCacheQuery {
                    action_digest: "abc:12".to_owned(),
                    ..Default::default()
                },
            )),
            Expect::Stage(Stage::LocalTestResultCacheQuery(
                buck2_data::LocalTestResultCacheQuery {
                    action_digest: "abc:12".to_owned(),
                    ..Default::default()
                },
            )),
            Expect::Error,
        )
        .await?;
        check_stage_payload(
            "24 then 25 (upstream's, with a phase)",
            [wire_field(24, &[]), wire_field(25, &[0x08, 0x01])].concat(),
            Expect::Stage(cas(buck2_data::cas_artifact_stage::Phase::CheckExpiration)),
            Expect::Stage(cas(buck2_data::cas_artifact_stage::Phase::CheckExpiration)),
            Expect::Stage(cas(buck2_data::cas_artifact_stage::Phase::CheckExpiration)),
        )
        .await
    }

    // ---- JSON ----

    fn json_line(data: buck2_data::buck_event::Data) -> serde_json::Value {
        serde_json::to_value(StreamValue::Event(Box::new(buck2_data::BuckEvent {
            trace_id: TRACE_ID.to_owned(),
            data: Some(data),
            ..Default::default()
        })))
        .unwrap()
    }

    /// Removes the members b1100380 does not have from the object at `pointer`: b1100380 writes
    /// the same JSON as the current writer, member for member, less these (the build.rs serde
    /// attributes of these messages are the same in both).
    fn as_b1100380(mut line: serde_json::Value, pointer: &str, upstream_only: &[&str]) -> String {
        let object = line
            .pointer_mut(pointer)
            .and_then(|v| v.as_object_mut())
            .unwrap_or_else(|| panic!("no object at {pointer}"));
        for member in upstream_only {
            assert!(object.remove(*member).is_some(), "{member} at {pointer}");
        }
        line.to_string()
    }

    /// The JSON header line: a writer from before the marker writes no `muninn_event_schema`
    /// member at all.
    fn header_json(muninn_event_schema: Option<u32>) -> String {
        let mut line = serde_json::to_value(header(muninn_event_schema)).unwrap();
        if muninn_event_schema.is_none() {
            assert!(
                line.as_object_mut()
                    .unwrap()
                    .remove("muninn_event_schema")
                    .is_some()
            );
        }
        line.to_string()
    }

    /// `ActionExecutionEnd` members upstream added after b1100380.
    const ACTION_EXECUTION_END_UPSTREAM_ONLY: &[&str] = &[
        "prepare_command_us",
        "finish_action_us",
        "graph_producers",
        "exec_group",
    ];
    /// `InvocationRecord` members upstream added after b1100380.
    const INVOCATION_RECORD_UPSTREAM_ONLY: &[&str] = &[
        "re_exceeded_quotas",
        "run_remote_served_from_cache_count",
        "run_remote_deduplicated_count",
        "agent_inferred_direct_call",
        "agent_callers",
        "agent_detection_stop_reason",
        "paging_arc_cache_entries",
        "build_duration_ms",
        "build_status",
        "paging_evictions",
        "paging_evicted_count",
        "paging_evicted_bytes",
        "paging_evictable_bytes",
        "settings_hash",
    ];

    /// A b1100380 JSON log, in the order of `b1100380_frames`. JSON keys members by name, so the
    /// stages and the record need no translation; only the enum, written as its number, does.
    fn b1100380_json_log() -> String {
        let end = "/Event/data/SpanEnd/data/ActionExecution";
        let action = |kind: i32| {
            let mut v = json_line(current_action_end(ActionExecutionKind::NotSet));
            *v.pointer_mut(&format!("{end}/execution_kind")).unwrap() = kind.into();
            as_b1100380(v, end, ACTION_EXECUTION_END_UPSTREAM_ONLY)
        };
        let mut critical_path = json_line(current_critical_path(&[
            ActionExecutionKind::NotSet,
            ActionExecutionKind::Remote,
        ]));
        *critical_path
            .pointer_mut("/Event/data/Instant/data/BuildGraphInfo/critical_path2/0/entry/ActionExecution/execution_kind")
            .unwrap() = 12.into();
        let record = "/Event/data/Record/data/InvocationRecord";
        let lines = [
            header_json(None),
            json_line(current_stage(Stage::LocalTestResultCacheQuery(query(
                "abc:12", "a1",
            ))))
            .to_string(),
            json_line(current_stage(Stage::LocalTestResultCacheQuery(query(
                "", "a2",
            ))))
            .to_string(),
            json_line(current_stage(Stage::LocalTestResultCache(
                legacy_hit().into(),
            )))
            .to_string(),
            action(b1100380::ActionExecutionKind::LocalTestResultCache as i32),
            critical_path.to_string(),
            as_b1100380(
                json_line(current_record(buck2_data::InvocationRecord {
                    run_local_count: 3,
                    run_local_test_result_cache_count: Some(5),
                    ..Default::default()
                })),
                record,
                INVOCATION_RECORD_UPSTREAM_ONLY,
            ),
            action(b1100380::ActionExecutionKind::Local as i32),
        ];
        lines.join("\n") + "\n"
    }

    #[tokio::test]
    async fn b1100380_json_log_reads_the_names_and_never_mislabels_12() -> buck2_error::Result<()> {
        let log = b1100380_json_log();
        assert!(!log.contains("muninn_event_schema"), "{log}");
        assert!(!log.contains("exec_group"), "{log}");
        assert!(log.contains("\"execution_kind\":12"), "{log}");
        for (reading, kind, kind_name) in [
            (UnmarkedLogReading::Ambiguous, AMBIGUOUS, AMBIGUOUS_NAME),
            (
                UnmarkedLogReading::LegacyFork,
                ActionExecutionKind::LocalTestResultCache as i32,
                "ACTION_EXECUTION_KIND_LOCAL_TEST_RESULT_CACHE",
            ),
        ] {
            let (invocation, values) = read(
                "log.json-lines",
                Encoding::JSON,
                log.clone().into_bytes(),
                reading,
            )
            .await?;
            assert_eq!(invocation.schema(), LogSchema::Unmarked);
            assert_eq!(values.len(), 7, "{reading:?}");
            assert_eq!(
                stage(&values[0]),
                &Stage::LocalTestResultCacheQuery(query("abc:12", "a1"))
            );
            // Named, so decided even with an empty digest.
            assert_eq!(
                stage(&values[1]),
                &Stage::LocalTestResultCacheQuery(query("", "a2"))
            );
            assert_eq!(
                stage(&values[2]),
                &Stage::LocalTestResultCache(legacy_hit().into())
            );
            assert_eq!(action_kind(&values[3]), kind, "{reading:?}");
            assert_eq!(
                critical_path_kinds(&values[4]),
                [(kind, kind_name), (2, "ACTION_EXECUTION_KIND_REMOTE")]
            );
            let r = record(&values[5]);
            assert_eq!(r.run_local_test_result_cache_count, Some(5));
            assert_eq!(r.run_local_count, 3);
            assert!(r.re_exceeded_quotas.is_empty());
            assert_eq!(action_kind(&values[6]), ActionExecutionKind::Local as i32);
        }
        Ok(())
    }

    /// Upstream-style unmarked JSON: 12 is ambiguous by default; the marked log with the same
    /// events reads 12 as served from cache, under either reading.
    #[tokio::test]
    async fn json_kind_12_is_ambiguous_unmarked_and_exact_marked() -> buck2_error::Result<()> {
        let events = [
            json_line(current_action_end(
                ActionExecutionKind::RemoteServedFromCache,
            )),
            json_line(current_stage(cas(
                buck2_data::cas_artifact_stage::Phase::Unspecified,
            ))),
        ];
        let log = |marker| {
            let mut lines = vec![header_json(marker)];
            lines.extend(events.iter().map(|e| e.to_string()));
            lines.join("\n") + "\n"
        };
        let marked = log(Some(MUNINN_EVENT_SCHEMA_CURRENT));
        assert!(
            marked
                .lines()
                .next()
                .unwrap()
                .contains("\"muninn_event_schema\":2")
        );

        let (_, values) = read(
            "log.json-lines",
            Encoding::JSON,
            log(None).into_bytes(),
            UnmarkedLogReading::Ambiguous,
        )
        .await?;
        assert_eq!(action_kind(&values[0]), AMBIGUOUS);
        // Named in JSON, so not ambiguous there.
        assert_eq!(
            stage(&values[1]),
            &cas(buck2_data::cas_artifact_stage::Phase::Unspecified)
        );

        for reading in [
            UnmarkedLogReading::Ambiguous,
            UnmarkedLogReading::LegacyFork,
        ] {
            let (invocation, values) = read(
                "log.json-lines",
                Encoding::JSON,
                marked.clone().into_bytes(),
                reading,
            )
            .await?;
            assert_eq!(invocation.schema(), LogSchema::Marked(2));
            assert_eq!(
                action_kind(&values[0]),
                ActionExecutionKind::RemoteServedFromCache as i32
            );
        }
        Ok(())
    }
}
