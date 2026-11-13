/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use buck2_error::BuckErrorOptionContext;
use buck2_error::internal_error;
use buck2_events::BuckEvent;
use buck2_wrapper_common::invocation_id::TraceId;

use crate::action_stats::ActionStats;
use crate::debug_events::DebugEventsState;
use crate::dice_state::DiceState;
use crate::progress::BuildProgressStateTracker;
use crate::re_state::ReState;
use crate::session_info::SessionInfo;
use crate::span_tracker::BuckEventSpanTracker;
use crate::starlark_debug::StarlarkDebuggerState;
use crate::test_state::TestState;
use crate::two_snapshots::TwoSnapshots;

pub struct EventObserver<E> {
    pub span_tracker: BuckEventSpanTracker,
    pub action_stats: ActionStats,
    re_state: ReState,
    two_snapshots: TwoSnapshots, // NOTE: We got many more copies of this than we should.
    system_info: buck2_data::SystemInfo,
    session_info: SessionInfo,
    test_state: TestState,
    starlark_debugger_state: StarlarkDebuggerState,
    dice_state: DiceState,
    /// When running without the Superconsole, we skip some state that we don't need. This might be
    /// premature optimization.
    extra: E,
}

impl<E> EventObserver<E>
where
    E: EventObserverExtra,
{
    pub fn new(trace_id: TraceId) -> Self {
        Self {
            span_tracker: BuckEventSpanTracker::new(),
            action_stats: ActionStats::default(),
            re_state: ReState::new(),
            two_snapshots: TwoSnapshots::default(),
            system_info: buck2_data::SystemInfo::default(),
            session_info: SessionInfo {
                trace_id: trace_id.clone(),
                test_session: None,
                legacy_dice: false,
            },
            test_state: TestState::default(),
            starlark_debugger_state: StarlarkDebuggerState::new(),
            dice_state: DiceState::new(),
            extra: E::new(),
        }
    }

    pub async fn observe(&mut self, event: &Arc<BuckEvent>) -> buck2_error::Result<()> {
        self.span_tracker.handle_event(event)?;

        {
            use buck2_data::buck_event::Data::*;

            match event.data() {
                SpanEnd(end) => {
                    use buck2_data::span_end_event::Data::*;

                    match end
                        .data
                        .as_ref()
                        .internal_error("Missing `data` in SpanEnd")?
                    {
                        ActionExecution(action_execution_end) => {
                            self.action_stats.update(action_execution_end);
                        }
                        TestRun(test_run_end) => {
                            self.action_stats.update_test_run(test_run_end);
                        }
                        _ => {}
                    }
                }
                Instant(instant) => {
                    use buck2_data::instant_event::Data::*;

                    match instant
                        .data
                        .as_ref()
                        .internal_error("Missing `data` in `Instant`")?
                    {
                        ReSession(re_session) => {
                            self.re_state.add_re_session(re_session);
                        }
                        Snapshot(snapshot) => {
                            self.re_state.update(snapshot);
                            self.two_snapshots.update(event.timestamp(), snapshot);
                        }
                        TestDiscovery(discovery) => {
                            use buck2_data::test_discovery::Data::*;

                            match discovery.data.as_ref().ok_or_else(|| {
                                internal_error!("Missing `data` in `TestDiscovery`")
                            })? {
                                Session(session) => {
                                    self.session_info.test_session = Some(session.clone());
                                }
                                Tests(tests) => {
                                    self.test_state.discovered += tests.test_names.len() as u64
                                }
                            }
                        }
                        TestResult(result) => {
                            self.test_state.update(result)?;
                        }
                        DebugAdapterSnapshot(snapshot) => {
                            self.starlark_debugger_state
                                .update(event.timestamp(), snapshot)?;
                        }
                        TagEvent(tags) => {
                            if tags.tags.contains(&"which-dice:Legacy".to_owned()) {
                                self.session_info.legacy_dice = true;
                            }
                        }
                        DiceStateSnapshot(dice) => {
                            self.dice_state.update(dice);
                        }
                        SystemInfo(system_info) => {
                            self.system_info = system_info.clone();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        self.extra.observe(event)?;

        Ok(())
    }

    pub fn spans(&self) -> &BuckEventSpanTracker {
        &self.span_tracker
    }

    pub fn action_stats(&self) -> &ActionStats {
        &self.action_stats
    }

    pub fn re_state(&self) -> &ReState {
        &self.re_state
    }

    pub fn two_snapshots(&self) -> &TwoSnapshots {
        &self.two_snapshots
    }

    pub fn system_info(&self) -> &buck2_data::SystemInfo {
        &self.system_info
    }

    pub fn session_info(&self) -> &SessionInfo {
        &self.session_info
    }

    pub fn starlark_debugger_state(&self) -> &StarlarkDebuggerState {
        &self.starlark_debugger_state
    }

    pub fn test_state(&self) -> &TestState {
        &self.test_state
    }

    pub fn extra(&self) -> &E {
        &self.extra
    }

    pub fn dice_state(&self) -> &DiceState {
        &self.dice_state
    }
}

pub trait EventObserverExtra: Send {
    fn new() -> Self;

    fn observe(&mut self, event: &Arc<BuckEvent>) -> buck2_error::Result<()>;
}

/// This has more fields for debug info. We don't always capture those.
pub struct DebugEventObserverExtra {
    debug_events: DebugEventsState,
    progress_state: BuildProgressStateTracker,
}

impl EventObserverExtra for DebugEventObserverExtra {
    fn new() -> Self {
        Self {
            debug_events: DebugEventsState::new(),
            progress_state: BuildProgressStateTracker::new(),
        }
    }

    fn observe(&mut self, event: &Arc<BuckEvent>) -> buck2_error::Result<()> {
        self.debug_events.handle_event(event)?;
        self.progress_state.handle_event(event)?;

        Ok(())
    }
}

impl DebugEventObserverExtra {
    pub fn debug_events(&self) -> &DebugEventsState {
        &self.debug_events
    }

    pub fn progress_state(&self) -> &BuildProgressStateTracker {
        &self.progress_state
    }
}

pub struct NoopEventObserverExtra;

impl EventObserverExtra for NoopEventObserverExtra {
    fn new() -> Self {
        Self
    }

    fn observe(&mut self, _event: &Arc<BuckEvent>) -> buck2_error::Result<()> {
        // Noop
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::SystemTime;

    use buck2_data::buck_event::Data;
    use buck2_data::command_execution_kind::Command;
    use buck2_events::BuckEvent;
    use buck2_events::span::SpanId;
    use buck2_wrapper_common::invocation_id::TraceId;

    use super::EventObserver;
    use super::NoopEventObserverExtra;

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

    fn stage(
        span: u64,
        parent: u64,
        stage: buck2_data::executor_stage_start::Stage,
    ) -> [Arc<BuckEvent>; 2] {
        [
            start(
                span,
                Some(parent),
                buck2_data::ExecutorStageStart { stage: Some(stage) }.into(),
            ),
            end(
                span,
                Some(parent),
                buck2_data::ExecutorStageEnd::default().into(),
            ),
        ]
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

    /// The events `BuckTestOrchestrator::execute_request` (via `execute_local_case`) emits for
    /// one test stage: a `TestRun` span containing the local-test-result-cache query stage, on a
    /// hit the hit stage, and a `TestRunEnd` carrying the final command report.
    fn test_run(span: u64, hit: bool) -> Vec<Arc<BuckEvent>> {
        let suite = buck2_data::TestSuite {
            suite_name: "suite".to_owned(),
            test_names: vec!["case".to_owned()],
            target_label: None,
        };
        let mut events = vec![start(
            span,
            None,
            buck2_data::TestRunStart {
                suite: Some(suite.clone()),
            }
            .into(),
        )];
        events.extend(stage(
            span + 1,
            span,
            buck2_data::LocalTestResultCacheQuery {
                action_digest: "digest".to_owned(),
                cache_key: "key".to_owned(),
                current_attempt: "current".to_owned(),
            }
            .into(),
        ));
        let command = if hit {
            let hit = buck2_data::LocalTestResultCache {
                action_digest: "digest".to_owned(),
                cache_key: "key".to_owned(),
                origin_attempt: "origin".to_owned(),
                entry_manifest: "manifest".to_owned(),
                current_attempt: "current".to_owned(),
            };
            events.extend(stage(span + 2, span, hit.clone().into()));
            Command::LocalTestResultCache(hit)
        } else {
            Command::LocalCommand(buck2_data::LocalCommand::default())
        };
        events.push(end(
            span,
            None,
            buck2_data::TestRunEnd {
                suite: Some(suite),
                command_report: Some(command_report(command)),
                command_host_sharing_requirements: None,
            }
            .into(),
        ));
        events
    }

    fn local_action(span: u64) -> [Arc<BuckEvent>; 2] {
        [
            start(
                span,
                None,
                buck2_data::ActionExecutionStart::default().into(),
            ),
            end(
                span,
                None,
                buck2_data::span_end_event::Data::ActionExecution(Box::new(
                    buck2_data::ActionExecutionEnd {
                        kind: buck2_data::ActionKind::Run as i32,
                        commands: vec![command_report(Command::LocalCommand(
                            buck2_data::LocalCommand::default(),
                        ))],
                        ..Default::default()
                    },
                )),
            ),
        ]
    }

    fn observe(observer: &mut EventObserver<NoopEventObserverExtra>, event: &Arc<BuckEvent>) {
        // The observer never awaits anything for these events.
        let mut fut = pin!(observer.observe(event));
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(r) => r.unwrap(),
            Poll::Pending => panic!("observer unexpectedly pending"),
        }
    }

    #[test]
    fn local_test_result_reuse_is_counted_from_test_run_end() {
        let mut observer = EventObserver::<NoopEventObserverExtra>::new(TraceId::null());
        for e in local_action(1)
            .iter()
            .chain(&test_run(10, true))
            .chain(&test_run(20, false))
        {
            observe(&mut observer, e);
        }
        let stats = observer.action_stats();
        // The hit is counted once, although both its hit stage and `TestRunEnd` were seen.
        assert_eq!(stats.local_test_result_cached_actions, 1);
        // The build action is accounted exactly as before; the fresh test run changes nothing.
        assert_eq!(stats.local_actions, 1);
        assert_eq!(stats.remote_actions, 0);
        assert_eq!(stats.cached_actions, 0);
        assert_eq!(stats.remote_dep_file_cached_actions, 0);
        assert_eq!(stats.fallback_actions, 0);
        // Build-action totals and cache-hit rate count the build action only.
        assert_eq!(stats.total_executed_and_cached_actions(), 1);
        assert_eq!(stats.total_cached_actions(), 0);
        assert_eq!(stats.total_cache_hit_percentage(), 0);
    }

    #[test]
    fn fresh_test_run_is_not_counted() {
        let mut observer = EventObserver::<NoopEventObserverExtra>::new(TraceId::null());
        for e in &test_run(10, false) {
            observe(&mut observer, e);
        }
        let stats = observer.action_stats();
        assert_eq!(stats.local_test_result_cached_actions, 0);
        assert_eq!(stats.total_executed_and_cached_actions(), 0);
    }
}
