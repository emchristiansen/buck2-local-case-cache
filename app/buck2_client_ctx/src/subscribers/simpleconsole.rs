/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Cow;
use std::fmt::Display;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use async_trait::async_trait;
use buck2_error::conversion::from_any_with_tag;
use buck2_event_observer::display;
use buck2_event_observer::display::TargetDisplayOptions;
use buck2_event_observer::display::display_file_watcher_end;
use buck2_event_observer::event_observer::EventObserver;
use buck2_event_observer::event_observer::EventObserverExtra;
use buck2_event_observer::humanized::HumanizedBytes;
use buck2_event_observer::unpack_event::VisitorError;
use buck2_event_observer::unpack_event::unpack_event;
use buck2_event_observer::verbosity::Verbosity;
use buck2_event_observer::what_ran::WhatRanCommandConsoleFormat;
use buck2_event_observer::what_ran::WhatRanOutputCommand;
use buck2_event_observer::what_ran::WhatRanOutputWriter;
use buck2_events::BuckEvent;
use buck2_hash::BuckMutMap;
use buck2_health_check::interface::HealthCheckType;
use buck2_health_check::report::DisplayReport;
use buck2_wrapper_common::invocation_id::TraceId;
use dupe::Dupe;
use superconsole::DrawMode;
use superconsole::SuperConsole;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;

use crate::agent_context::AgentContextEntry;
use crate::subscribers::console_output_limit::ConsoleOutputLimit;
use crate::subscribers::console_output_limit::EmitResult;
use crate::subscribers::emit_event::emit_event_if_relevant;
use crate::subscribers::subscriber::EventSubscriber;
use crate::subscribers::superconsole::io::io_in_flight_non_zero_counters;
use crate::subscribers::system_warning::check_memory_pressure_snapshot;
use crate::subscribers::system_warning::check_remaining_disk_space_snapshot;
use crate::subscribers::system_warning::low_disk_space_msg;
use crate::subscribers::system_warning::system_memory_exceeded_msg;
use crate::ticker::Tick;

/// buck2 daemon info is printed to stderr if there are no other updates available
/// within this duration.
const KEEPALIVE_TIME_LIMIT: Duration = Duration::from_secs(7);

/// The shortest gap between two "Waiting on ..." lines when an AI coding agent runs
/// buck2. Agents read the output only after the command exits, so the line every 7s
/// mostly adds tokens to their context. Health, resource and stale-snapshot reports
/// keep the 7s cadence.
const AGENT_WAITING_LINE_INTERVAL: Duration = Duration::from_secs(60);

/// Rate-limits the "Waiting on ..." keepalive line: agents get it at most once per
/// `AGENT_WAITING_LINE_INTERVAL`, everyone else on every keepalive tick.
struct WaitingLineThrottle {
    min_interval: Duration,
    last_shown: Instant,
}

impl WaitingLineThrottle {
    fn new(is_agent: bool, now: Instant) -> Self {
        Self {
            min_interval: if is_agent {
                AGENT_WAITING_LINE_INTERVAL
            } else {
                Duration::ZERO
            },
            last_shown: now,
        }
    }

    fn should_show(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_shown) < self.min_interval {
            return false;
        }
        self.last_shown = now;
        true
    }
}

fn is_agent_invocation() -> bool {
    AgentContextEntry::from_env().is_ok_and(|entries| !entries.is_empty())
}

fn now_display() -> impl Display {
    // Millisecond precision with a numeric offset, like the rfc3339 formatting this had
    // historically.
    jiff::Zoned::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.3f%:z")
        .to_string()
}

fn with_timestamps(message: &str) -> String {
    let mut s = String::new();
    let now = now_display();
    for line in message.lines() {
        if line.is_empty() {
            writeln!(s, "[{now}]").unwrap();
        } else {
            writeln!(s, "[{now}] {line}").unwrap();
        }
    }
    // Remove the trailing newline
    s.pop();
    s
}

// Echoes a message to stderr, along with a timestamp.
macro_rules! echo {
    () => {
        {
            crate::eprintln!("[{}]", now_display())
        }
    };
    ($fmt:expr $(, $args:expr)*) => {
        {
            let message = format!($fmt $(, $args)*);
            let message = with_timestamps(&message);
            crate::eprintln!("{}", message)
        }
    };
}

#[derive(Copy, Clone, Dupe, Debug, PartialEq)]
enum TtyMode {
    Enabled,
    Disabled,
}

struct HealthWarningBackoff {
    elapsed: BuckMutMap<HealthCheckType, (Instant, u64)>,
}

impl HealthWarningBackoff {
    fn new(now: Instant) -> Self {
        let elapsed = [
            HealthCheckType::MemoryPressure,
            HealthCheckType::LowDiskSpace,
            HealthCheckType::SlowDownloadSpeed,
            HealthCheckType::VpnEnabled,
            HealthCheckType::StableRevision,
            HealthCheckType::SlowBuild,
        ]
        .into_iter()
        .map(|warning| (warning, (now, 1)))
        .collect();
        Self { elapsed }
    }

    // Report only if at least double the previous reporting interval has passed.
    fn should_display(&mut self, warning: &HealthCheckType, now: Instant) -> bool {
        let Some((last_reported, every_x)) = self.elapsed.get_mut(warning) else {
            return false;
        };
        let new_every_double = 2 * *every_x;
        if now.duration_since(*last_reported) <= Duration::from_secs(new_every_double) {
            return false;
        }
        *every_x = new_every_double;
        *last_reported = now;
        true
    }
}

fn echo_system_warning_exponential(
    backoff: &mut HealthWarningBackoff,
    warning: &HealthCheckType,
    message: &str,
) -> buck2_error::Result<()> {
    if backoff.should_display(warning, Instant::now()) {
        echo!("{}", message)?;
    }
    Ok(())
}

/// Just repeats stdout and stderr to client process.
pub struct SimpleConsole<E> {
    tty_mode: TtyMode,
    verbosity: Verbosity,
    // Whether to show "Waiting for daemon..." when no root spans are received
    expect_spans: bool,
    pub(crate) observer: EventObserver<E>,
    action_errors: Vec<buck2_data::ActionError>,
    last_print_time: Instant,
    waiting_line: WaitingLineThrottle,
    last_shown_snapshot_ts: Option<SystemTime>,
    health_check_reports_receiver: Option<Receiver<Vec<DisplayReport>>>,
    health_warning_backoff: HealthWarningBackoff,
    pub(crate) output_limit: ConsoleOutputLimit,
}

impl<E> SimpleConsole<E>
where
    E: EventObserverExtra,
{
    pub(crate) fn with_tty(
        trace_id: TraceId,
        verbosity: Verbosity,
        expect_spans: bool,
        health_check_reports_receiver: Option<Receiver<Vec<DisplayReport>>>,
    ) -> Self {
        let now = Instant::now();
        SimpleConsole {
            tty_mode: TtyMode::Enabled,
            verbosity,
            expect_spans,
            observer: EventObserver::new(trace_id),
            action_errors: Vec::new(),
            last_print_time: now,
            waiting_line: WaitingLineThrottle::new(is_agent_invocation(), now),
            last_shown_snapshot_ts: None,
            health_check_reports_receiver,
            health_warning_backoff: HealthWarningBackoff::new(now),
            output_limit: ConsoleOutputLimit::new(),
        }
    }

    pub(crate) fn without_tty(
        trace_id: TraceId,
        verbosity: Verbosity,
        expect_spans: bool,
        health_check_reports_receiver: Option<Receiver<Vec<DisplayReport>>>,
    ) -> Self {
        let now = Instant::now();
        SimpleConsole {
            tty_mode: TtyMode::Disabled,
            verbosity,
            expect_spans,
            observer: EventObserver::new(trace_id),
            action_errors: Vec::new(),
            last_print_time: now,
            waiting_line: WaitingLineThrottle::new(is_agent_invocation(), now),
            last_shown_snapshot_ts: None,
            health_check_reports_receiver,
            health_warning_backoff: HealthWarningBackoff::new(now),
            output_limit: ConsoleOutputLimit::new(),
        }
    }

    /// Create a SimpleConsole that auto detects whether it has a TTY or not.
    pub(crate) fn autodetect(
        trace_id: TraceId,
        verbosity: Verbosity,
        expect_spans: bool,
        health_check_reports_receiver: Option<Receiver<Vec<DisplayReport>>>,
    ) -> Self {
        match SuperConsole::compatible() {
            true => Self::with_tty(
                trace_id,
                verbosity,
                expect_spans,
                health_check_reports_receiver,
            ),
            false => Self::without_tty(
                trace_id,
                verbosity,
                expect_spans,
                health_check_reports_receiver,
            ),
        }
    }

    pub(crate) fn observer(&self) -> &EventObserver<E> {
        &self.observer
    }

    pub(crate) async fn update_event_observer(
        &mut self,
        event: &Arc<BuckEvent>,
    ) -> buck2_error::Result<()> {
        self.observer.observe(event).await
    }

    fn notify_printed(&mut self) {
        self.last_print_time = Instant::now();
    }

    fn print_stats_while_waiting(&mut self) -> buck2_error::Result<()> {
        let snapshots = self.observer().two_snapshots();

        if let Some(h) = self
            .observer()
            .re_state()
            .render_header(snapshots, DrawMode::Normal)
        {
            echo!("{}", h)?;
        }

        let last_snapshot_ts = snapshots.last.as_ref().map(|(ts, _)| *ts);

        // We normally send snapshots more often than we print this, so we'd expect the
        // snapshot to change on every call.
        let is_snapshot_stale = match (&self.last_shown_snapshot_ts, last_snapshot_ts) {
            (Some(x), Some(y)) => *x == y,
            _ => false,
        };

        if is_snapshot_stale {
            echo!("Resource usage: <snapshot is stale>")?;
        } else {
            let mut parts = Vec::with_capacity(2);
            if let Some((_, snapshot)) = &snapshots.last {
                if let Some(buck2_rss) = snapshot.buck2_rss {
                    parts.push(format!("RSS: {}", HumanizedBytes::new(buck2_rss)));
                }
            }
            if let Some(cpu) = snapshots.cpu_percents() {
                parts.push(format!("CPU: {cpu}%"));
            }
            if !parts.is_empty() {
                echo!("Resource usage: {}", parts.join(" "))?;
            }

            if let Some((_ts, snapshot)) = &snapshots.last {
                let mut parts = Vec::new();
                for (key, value) in io_in_flight_non_zero_counters(snapshot) {
                    parts.push(format!("{key:?}: {value}"));
                }
                if !parts.is_empty() {
                    echo!("IO: {}", parts.join(" "))?;
                } else {
                    echo!("IO: none")?;
                }
            }

            self.last_shown_snapshot_ts = last_snapshot_ts;
        }

        Ok(())
    }

    fn print_action_error(&mut self, error: &buck2_data::ActionError) -> buck2_error::Result<()> {
        let error_display = display::display_action_error(error, TargetDisplayOptions::for_log())?;
        let stream_bytes = error_display.output_stream_byte_count();

        // Always print metadata (action ID, reason, command).
        // Gate stdout/stderr through the output limit.
        let output_format = match self.output_limit.emit(stream_bytes) {
            EmitResult::Emit => display::ActionErrorOutputFormat::IncludeOutputStreams,
            EmitResult::Exceeded(msg) => {
                display::ActionErrorOutputFormat::SubstituteOutputStreams(msg)
            }
            EmitResult::Skipped => display::ActionErrorOutputFormat::ExcludeOutputStreams,
        };

        let message = error_display.simple_format_with_timestamps(with_timestamps, output_format);
        if self.tty_mode == TtyMode::Disabled {
            // ast-grep-ignore: rust/buck2-cli-simpleconsole-echo
            crate::eprintln!("{}", display::sanitize_output_colors(message.as_bytes()))?;
        } else {
            // ast-grep-ignore: rust/buck2-cli-simpleconsole-echo
            crate::eprintln!("{}", message)?;
        }

        self.notify_printed();
        Ok(())
    }

    pub(crate) async fn handle_file_watcher_end(
        &mut self,
        file_watcher: &buck2_data::FileWatcherEnd,
        _event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        if self.verbosity.print_status() {
            for x in display_file_watcher_end(file_watcher) {
                echo!("{}", x)?;
            }
            self.notify_printed();
        }
        Ok(())
    }

    pub(crate) async fn handle_event(&mut self, event: &Arc<BuckEvent>) -> buck2_error::Result<()> {
        self.update_event_observer(event).await?;

        self.handle_event_inner(event).await?;

        if self.verbosity.print_all_commands() {
            emit_event_if_relevant(
                event.parent_id().into(),
                event.data(),
                self.observer().spans(),
                &mut PrintDebugCommandToStderr,
            )?;
        }

        Ok(())
    }

    async fn handle_event_inner(&mut self, event: &BuckEvent) -> buck2_error::Result<()> {
        match unpack_event(event)? {
            buck2_event_observer::unpack_event::UnpackedBuckEvent::SpanStart(_, _, data) => {
                match data {
                    buck2_data::span_start_event::Data::Command(command) => {
                        self.handle_command_start(command, event).await
                    }
                    _ => Ok(()),
                }
            }
            buck2_event_observer::unpack_event::UnpackedBuckEvent::SpanEnd(_, _, data) => {
                match data {
                    buck2_data::span_end_event::Data::Command(command) => {
                        self.handle_command_end(command, event).await
                    }
                    buck2_data::span_end_event::Data::ActionExecution(action) => {
                        self.handle_action_execution_end(action, event).await
                    }
                    buck2_data::span_end_event::Data::FileWatcher(file_watcher) => {
                        self.handle_file_watcher_end(file_watcher, event).await
                    }
                    _ => Ok(()),
                }
            }
            buck2_event_observer::unpack_event::UnpackedBuckEvent::Instant(_, _, data) => {
                match data {
                    buck2_data::instant_event::Data::ConsoleMessage(message) => {
                        self.handle_stderr(&message.message).await
                    }
                    buck2_data::instant_event::Data::ConsoleWarning(message) => {
                        self.handle_stderr(&message.message).await
                    }
                    buck2_data::instant_event::Data::ReSession(session) => {
                        let message = format!("RE Session: {}", session.session_id);
                        self.handle_stderr(&message).await
                    }
                    buck2_data::instant_event::Data::StructuredError(err) => {
                        self.handle_structured_error(err, event).await
                    }
                    buck2_data::instant_event::Data::TestDiscovery(discovery) => {
                        self.handle_test_discovery(discovery, event).await
                    }
                    buck2_data::instant_event::Data::TestResult(result) => {
                        self.handle_test_result(result, event).await
                    }
                    buck2_data::instant_event::Data::TagEvent(tags) => {
                        if tags.tags.contains(&"which-dice:Legacy".to_owned()) {
                            self.handle_stderr("Note: using deprecated legacy dice.")
                                .await?;
                        }

                        Ok(())
                    }
                    buck2_data::instant_event::Data::ActionError(error) => {
                        self.handle_action_error(error).await
                    }
                    buck2_data::instant_event::Data::StreamingOutput(message) => {
                        crate::stdio::print_bytes(message.message.as_bytes())?;
                        crate::stdio::flush()?;
                        self.notify_printed();
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            buck2_event_observer::unpack_event::UnpackedBuckEvent::UnrecognizedSpanStart(_, _)
            | buck2_event_observer::unpack_event::UnpackedBuckEvent::UnrecognizedSpanEnd(_, _)
            | buck2_event_observer::unpack_event::UnpackedBuckEvent::UnrecognizedInstant(_, _) => {
                Err(VisitorError::MissingField(event.clone()).into())
            }
        }
    }

    pub(crate) async fn handle_structured_error(
        &mut self,
        err: &buck2_data::StructuredError,
        _event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        if err.quiet {
            return Ok(());
        }
        echo!("{}", err.payload)?;
        self.notify_printed();
        Ok(())
    }

    async fn handle_command_start(
        &mut self,
        _command: &buck2_data::CommandStart,
        event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        if cfg!(fbcode_build) {
            echo!(
                "Buck UI: https://www.internalfb.com/buck2/{}",
                event.trace_id()?
            )?;
        } else {
            echo!("Build ID: {}", event.trace_id()?)?;
        }
        self.notify_printed();
        Ok(())
    }

    async fn handle_command_end(
        &mut self,
        _command: &buck2_data::CommandEnd,
        _event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        let snapshots = self.observer().two_snapshots();

        if self.verbosity.print_status() {
            let action_stats = self.observer().action_stats();
            // The build cache-hit rate and command counts describe build actions only. Without a
            // build action that executed or hit a cache there is no rate to report (the shared
            // zero-denominator helper would claim 100%), even when test results were reused.
            if action_stats.total_executed_and_cached_actions() > 0 {
                echo!("Cache hits: {}%", action_stats.total_cache_hit_percentage())?;
                echo!(
                    "Commands: {} (cached: {}, remote: {}, local: {})",
                    action_stats.total_executed_and_cached_actions(),
                    action_stats.cached_actions,
                    action_stats.remote_actions,
                    action_stats.local_actions
                )?;
                if action_stats.fallback_actions > 0 {
                    echo!(
                        "Fallback: {}/{}",
                        action_stats.fallback_actions,
                        action_stats.total_executed_actions()
                    )?;
                }
            }
            // Reused test results are not build commands: they get their own line rather than
            // entering the command or cache counts above.
            if action_stats.local_test_result_cached_actions > 0 {
                echo!(
                    "Local test-result reuse: {}",
                    action_stats.local_test_result_cached_actions
                )?;
            }
        }

        if let Some(re) = self
            .observer()
            .re_state()
            .render_header(snapshots, DrawMode::Final)
        {
            echo!("{}", re)?;
        }

        if let Some(test_session) = &self.observer().session_info().test_session {
            echo!("Test session: {}", test_session.info)?;
        }

        Ok(())
    }

    pub(crate) async fn handle_action_execution_end(
        &mut self,
        action: &buck2_data::ActionExecutionEnd,
        _event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        let action_id = display::display_action_identity(
            action.key.as_ref(),
            action.name.as_ref(),
            TargetDisplayOptions::for_log(),
        )?;

        let stderr = display::success_stderr(action, self.verbosity)?;

        if self.verbosity.print_all_actions() || stderr.is_some() {
            let complete = self.observer().spans().roots_completed();
            let incomplete = self.observer().spans().roots_ongoing();
            echo!("{} / {}: {}", complete, complete + incomplete, action_id)?;
            if let Some(stderr) = stderr {
                match self.output_limit.emit(stderr.len()) {
                    EmitResult::Emit => {
                        // TODO(nmj): Factor out behavior here so that handling ttymode isn't ad hoc.  i.e. write a method that formats text based on tty mode
                        match self.tty_mode {
                            TtyMode::Enabled => {
                                // Add the extra control character so that users' stderr messages can't
                                // mess up the terminal
                                echo!("stderr:{}\x1b[0m", stderr)?;
                            }
                            TtyMode::Disabled => {
                                echo!(
                                    "stderr:\n{}",
                                    display::sanitize_output_colors(stderr.as_bytes())
                                )?;
                            }
                        }
                    }
                    EmitResult::Exceeded(msg) => {
                        echo!("{}", msg)?;
                    }
                    EmitResult::Skipped => {}
                }
            }
            self.notify_printed();
        }

        Ok(())
    }

    pub(crate) async fn handle_action_error(
        &mut self,
        error: &buck2_data::ActionError,
    ) -> buck2_error::Result<()> {
        self.print_action_error(error)?;
        self.action_errors.push(error.clone());
        Ok(())
    }

    async fn handle_test_discovery(
        &mut self,
        test_info: &buck2_data::TestDiscovery,
        _event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        if let Some(data) = &test_info.data {
            match data {
                buck2_data::test_discovery::Data::Session(buck2_data::TestSessionInfo {
                    info,
                    ..
                }) => {
                    echo!("Test session: {}", info)?;
                    self.notify_printed();
                }
                buck2_data::test_discovery::Data::Tests(..) => {}
            }
        }

        Ok(())
    }

    async fn handle_test_result(
        &mut self,
        result: &buck2_data::TestResult,
        _event: &BuckEvent,
    ) -> buck2_error::Result<()> {
        if let Some(msg) = display::format_test_result(result, self.verbosity)? {
            let mut buffer = String::new();

            for line in msg {
                writeln!(buffer, "{}", line.to_unstyled())
                    .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))?;
            }
            match self.output_limit.emit(buffer.len()) {
                EmitResult::Emit => {
                    echo!("{}", buffer)?;
                }
                EmitResult::Exceeded(msg) => {
                    echo!("{}", msg)?;
                }
                EmitResult::Skipped => {}
            }
        }

        Ok(())
    }

    pub(crate) async fn handle_stderr(&mut self, stderr: &str) -> buck2_error::Result<()> {
        echo!("{}", stderr)?;
        self.notify_printed();
        Ok(())
    }

    pub(crate) fn try_recv_health_check_display_reports(&mut self) -> Option<Vec<DisplayReport>> {
        if let Some(receiver) = self.health_check_reports_receiver.as_mut() {
            let mut reports = Vec::new();
            loop {
                match receiver.try_recv() {
                    Ok(report) => reports.extend(report),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // If the sender has been dropped, remove the receiver so that we can avoid trying again.
                        self.health_check_reports_receiver = None;
                        break;
                    }
                }
            }
            if !reports.is_empty() {
                return Some(reports);
            }
        }
        None
    }

    fn echo_health_check_warning(&mut self, report: &DisplayReport) -> buck2_error::Result<()> {
        if let Some(warning) = &report.health_issue {
            echo_system_warning_exponential(
                &mut self.health_warning_backoff,
                &report.health_check_type,
                &warning.to_string(),
            )?;
        }
        Ok(())
    }
}

#[async_trait]
impl<E> EventSubscriber for SimpleConsole<E>
where
    E: EventObserverExtra,
{
    async fn handle_output(&mut self, raw_output: &[u8]) -> buck2_error::Result<()> {
        // We expect output that gets here to already have been buffered if possible (because it
        // primarily gets to us through a GRPC layer that already needs buffering), so we
        // unconditionally flush it.
        crate::stdio::print_bytes(raw_output)?;
        crate::stdio::flush()?;
        self.notify_printed();
        Ok(())
    }

    async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> buck2_error::Result<()> {
        for ev in events {
            self.handle_event(ev).await?;
        }
        Ok(())
    }

    async fn handle_tailer_stderr(&mut self, stderr: &str) -> buck2_error::Result<()> {
        self.handle_stderr(stderr).await
    }

    async fn handle_command_result(
        &mut self,
        result: &buck2_cli_proto::CommandResult,
    ) -> buck2_error::Result<()> {
        let errors = std::mem::take(&mut self.action_errors);

        if !errors.is_empty() {
            echo!()?;
            echo!("BUILD ERRORS ({})", errors.len())?;
            echo!("The following actions failed during the execution of this command:")?;
            for error in errors.iter() {
                self.print_action_error(error)?;
            }
            echo!()?;
            self.notify_printed();
        }

        crate::subscribers::errorconsole::ErrorConsole
            .handle_command_result(result)
            .await
    }

    async fn tick(&mut self, _: &Tick) -> buck2_error::Result<()> {
        let now = Instant::now();
        if self.verbosity.print_status() && now - self.last_print_time > KEEPALIVE_TIME_LIMIT {
            let show_waiting_line = self.waiting_line.should_show(now);
            let mut show_stats = self.expect_spans;

            for report in self
                .try_recv_health_check_display_reports()
                .unwrap_or_default()
            {
                self.echo_health_check_warning(&report)?;
            }

            let mut roots = self.observer.spans().iter_roots();
            let sample_event = roots.next();
            match sample_event {
                Some(sample_event) => {
                    if show_waiting_line {
                        let child = match sample_event.children().next() {
                            Some(c) => Cow::Owned(format!(
                                " [{}]",
                                display::display_event(
                                    &c.info().event,
                                    TargetDisplayOptions::for_log(),
                                )?
                            )),
                            None => Cow::Borrowed(""),
                        };

                        let remaining = match roots.len() {
                            0 => String::new(),
                            x => format!(", and {x} other actions"),
                        };
                        echo!(
                            "Waiting on {}{}{}",
                            display::display_event(
                                &sample_event.info().event,
                                TargetDisplayOptions::for_log()
                            )?,
                            child,
                            remaining
                        )?;
                    }

                    let last_snapshot = self.observer.two_snapshots().last.as_ref().map(|s| &s.1);
                    let sysinfo = self.observer.system_info();
                    if let Some(memory_pressure) =
                        check_memory_pressure_snapshot(last_snapshot, sysinfo)
                    {
                        echo_system_warning_exponential(
                            &mut self.health_warning_backoff,
                            &HealthCheckType::MemoryPressure,
                            &system_memory_exceeded_msg(&memory_pressure),
                        )?;
                    }
                    if let Some(low_disk_space) =
                        check_remaining_disk_space_snapshot(last_snapshot, sysinfo)
                    {
                        echo_system_warning_exponential(
                            &mut self.health_warning_backoff,
                            &HealthCheckType::LowDiskSpace,
                            &low_disk_space_msg(&low_disk_space),
                        )?;
                    }
                    show_stats = self.verbosity.always_print_stats_in_status();
                }
                None => {
                    if self.expect_spans && show_waiting_line {
                        echo!(
                            "Waiting on buck2 daemon {}...",
                            self.observer.session_info().trace_id
                        )?;
                    }
                }
            }
            // roots must be dropped here because it mutably borrows `self`
            // and doesn't get dropped until the end of this scope otherwise.
            std::mem::drop(roots);

            if show_stats {
                self.print_stats_while_waiting()?;
            }

            self.notify_printed();
        }

        Ok(())
    }

    async fn handle_error(&mut self, _error: &buck2_error::Error) -> buck2_error::Result<()> {
        // We don't need to do any cleanup to exit.
        Ok(())
    }
}

struct PrintDebugCommandToStderr;

impl WhatRanOutputWriter for PrintDebugCommandToStderr {
    fn emit_command(&mut self, command: WhatRanOutputCommand<'_>) -> buck2_error::Result<()> {
        echo!(
            "{}",
            WhatRanCommandConsoleFormat {
                reason: command.reason,
                identity: command.identity,
                repro: command.repro,
            }
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_warning_backoff_is_exponential_and_instance_local() {
        let start = Instant::now();
        let warning = HealthCheckType::MemoryPressure;
        let mut first = HealthWarningBackoff::new(start);
        let mut second = HealthWarningBackoff::new(start);

        assert!(!first.should_display(&warning, start + Duration::from_secs(2)));
        assert!(first.should_display(&warning, start + Duration::from_secs(3)));
        assert!(!first.should_display(&warning, start + Duration::from_secs(7)));
        assert!(second.should_display(&warning, start + Duration::from_secs(3)));
    }

    #[test]
    fn waiting_line_is_throttled_for_agents_only() {
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);

        let mut human = WaitingLineThrottle::new(false, start);
        assert!(
            human.should_show(at(8)) && human.should_show(at(16)),
            "humans get the line on every keepalive tick"
        );

        let mut agent = WaitingLineThrottle::new(true, start);
        assert!(
            !agent.should_show(at(8)) && !agent.should_show(at(59)),
            "agents skip the 7s keepalive ticks"
        );
        assert!(agent.should_show(at(60)), "agents get the line a minute in");
        assert!(
            !agent.should_show(at(100)),
            "and not again before another minute"
        );
        assert!(agent.should_show(at(120)));
    }

    mod command_end {
        use buck2_data::buck_event::Data;
        use buck2_data::command_execution_kind::Command;
        use buck2_event_observer::event_observer::NoopEventObserverExtra;
        use buck2_events::span::SpanId;

        use super::*;

        const COMMAND_SPAN: u64 = 1;

        fn event(span: u64, parent: Option<u64>, data: Data) -> Arc<BuckEvent> {
            Arc::new(BuckEvent::new(
                SystemTime::now(),
                TraceId::null(),
                Some(SpanId::from_u64(span).unwrap()),
                parent.map(|p| SpanId::from_u64(p).unwrap()),
                data,
            ))
        }

        fn start(
            span: u64,
            parent: Option<u64>,
            data: buck2_data::span_start_event::Data,
        ) -> Arc<BuckEvent> {
            event(
                span,
                parent,
                Data::SpanStart(buck2_data::SpanStartEvent { data: Some(data) }),
            )
        }

        fn end(
            span: u64,
            parent: Option<u64>,
            data: buck2_data::span_end_event::Data,
        ) -> Arc<BuckEvent> {
            event(
                span,
                parent,
                Data::SpanEnd(buck2_data::SpanEndEvent {
                    data: Some(data),
                    ..Default::default()
                }),
            )
        }

        fn command_report(command: Command) -> buck2_data::CommandExecution {
            buck2_data::CommandExecution {
                details: Some(buck2_data::CommandExecutionDetails {
                    command_kind: Some(buck2_data::CommandExecutionKind {
                        command: Some(command),
                    }),
                    ..Default::default()
                }),
                status: Some(buck2_data::command_execution::Success {}.into()),
                ..Default::default()
            }
        }

        /// A test run whose final command report is either a local test-result reuse or a fresh
        /// local execution, as `TestRunEnd` carries it.
        fn test_run(span: u64, reused: bool) -> Vec<Arc<BuckEvent>> {
            let suite = buck2_data::TestSuite {
                suite_name: format!("suite{span}"),
                test_names: vec!["case".to_owned()],
                target_label: None,
            };
            let command = if reused {
                Command::LocalTestResultCache(buck2_data::LocalTestResultCache {
                    action_digest: "digest".to_owned(),
                    cache_key: "key".to_owned(),
                    origin_attempt: "origin".to_owned(),
                    entry_manifest: "manifest".to_owned(),
                    current_attempt: "current".to_owned(),
                })
            } else {
                Command::LocalCommand(buck2_data::LocalCommand::default())
            };
            vec![
                start(
                    span,
                    Some(COMMAND_SPAN),
                    buck2_data::TestRunStart {
                        suite: Some(suite.clone()),
                    }
                    .into(),
                ),
                end(
                    span,
                    Some(COMMAND_SPAN),
                    buck2_data::TestRunEnd {
                        suite: Some(suite),
                        command_report: Some(command_report(command)),
                        command_host_sharing_requirements: None,
                    }
                    .into(),
                ),
            ]
        }

        fn action_key(span: u64) -> buck2_data::ActionKey {
            buck2_data::ActionKey {
                owner: Some(buck2_data::action_key::Owner::TargetLabel(
                    buck2_data::ConfiguredTargetLabel {
                        label: Some(buck2_data::TargetLabel {
                            package: "root//pkg".to_owned(),
                            name: format!("t{span}"),
                        }),
                        configuration: Some(buck2_data::Configuration {
                            full_name: "cfg".to_owned(),
                        }),
                        execution_configuration: None,
                    },
                )),
                ..Default::default()
            }
        }

        fn action_name() -> buck2_data::ActionName {
            buck2_data::ActionName {
                category: "cat".to_owned(),
                identifier: "id".to_owned(),
            }
        }

        /// A build action whose last command is `command`.
        fn build_action(span: u64, command: Command) -> Vec<Arc<BuckEvent>> {
            vec![
                start(
                    span,
                    Some(COMMAND_SPAN),
                    buck2_data::ActionExecutionStart {
                        key: Some(action_key(span)),
                        name: Some(action_name()),
                        kind: buck2_data::ActionKind::Run as i32,
                    }
                    .into(),
                ),
                end(
                    span,
                    Some(COMMAND_SPAN),
                    buck2_data::span_end_event::Data::ActionExecution(Box::new(
                        buck2_data::ActionExecutionEnd {
                            key: Some(action_key(span)),
                            name: Some(action_name()),
                            kind: buck2_data::ActionKind::Run as i32,
                            commands: vec![command_report(command)],
                            ..Default::default()
                        },
                    )),
                ),
            ]
        }

        /// Feeds a whole command through the real `SimpleConsole` event entry point, with status
        /// printing on, and returns the lines it printed at command end (timestamps removed).
        fn command_end_lines(body: Vec<Arc<BuckEvent>>) -> Vec<String> {
            let mut console = SimpleConsole::<NoopEventObserverExtra>::without_tty(
                TraceId::null(),
                Verbosity::try_from_cli("status").unwrap(),
                true,
                None,
            );
            assert!(console.verbosity.print_status());
            futures::executor::block_on(async {
                console
                    .handle_event(&start(
                        COMMAND_SPAN,
                        None,
                        buck2_data::CommandStart::default().into(),
                    ))
                    .await
                    .unwrap();
                for e in &body {
                    console.handle_event(e).await.unwrap();
                }
            });
            let ((), printed) = crate::stdio::capture_stderr(|| {
                futures::executor::block_on(console.handle_event(&end(
                    COMMAND_SPAN,
                    None,
                    buck2_data::CommandEnd::default().into(),
                )))
                .unwrap()
            });
            printed
                .lines()
                .map(|line| match line.split_once("] ") {
                    Some((_, message)) => message.to_owned(),
                    None => line.to_owned(),
                })
                .collect()
        }

        #[test]
        fn empty_command_prints_no_build_or_reuse_stats() {
            assert_eq!(command_end_lines(Vec::new()), Vec::<String>::new());
        }

        #[test]
        fn fresh_test_runs_only_print_no_build_or_reuse_stats() {
            let body = test_run(10, false);
            assert_eq!(command_end_lines(body), Vec::<String>::new());
        }

        #[test]
        fn reuse_only_command_prints_reuse_without_a_build_cache_rate() {
            let body = [test_run(10, true), test_run(20, true), test_run(30, false)].concat();
            assert_eq!(
                command_end_lines(body),
                vec!["Local test-result reuse: 2".to_owned()]
            );
        }

        #[test]
        fn mixed_command_prints_build_stats_and_separate_reuse() {
            let body = [
                build_action(
                    10,
                    Command::LocalCommand(buck2_data::LocalCommand::default()),
                ),
                build_action(
                    20,
                    Command::RemoteCommand(buck2_data::RemoteCommand {
                        cache_hit: true,
                        cache_hit_type: buck2_data::RemoteCommandOrigin::ActionCache.into(),
                        ..Default::default()
                    }),
                ),
                test_run(30, true),
                test_run(40, false),
            ]
            .concat();
            assert_eq!(
                command_end_lines(body),
                vec![
                    "Cache hits: 50%".to_owned(),
                    "Commands: 2 (cached: 1, remote: 0, local: 1)".to_owned(),
                    "Local test-result reuse: 1".to_owned(),
                ]
            );
        }
    }
}
