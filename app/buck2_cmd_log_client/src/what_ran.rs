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
use std::io::Write;

use buck2_client_ctx::client_ctx::BuckSubcommand;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::BuckArgMatches;
use buck2_client_ctx::event_log_options::EventLogOptions;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::exit_result::ClientIoError;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_data::SchedulingMode;
use buck2_data::re_platform::Property;
use buck2_error::conversion::from_any_with_tag;
use buck2_event_log::stream_value::StreamValue;
use buck2_event_observer::fmt_duration;
use buck2_event_observer::what_ran;
use buck2_event_observer::what_ran::CommandReproducer;
use buck2_event_observer::what_ran::WhatRanOptions;
use buck2_event_observer::what_ran::WhatRanOutputCommand;
use buck2_event_observer::what_ran::WhatRanOutputCommandExtra;
use buck2_event_observer::what_ran::WhatRanOutputWriter;
use buck2_event_observer::what_ran::WhatRanRelevantAction;
use buck2_event_observer::what_ran::WhatRanState;
use buck2_events::span::SpanId;
use buck2_hash::BuckMutInsertionMap;
use futures::TryStreamExt;
use futures::stream::Stream;

use crate::LogCommandOutputFormat;
use crate::LogCommandOutputFormatWithWriter;
use crate::OutputFormatWithWriter;
use crate::transform_format;

/// Output everything that buck ran from the selected invocation. If no invocation was specified,
/// use the last buck invocation from this isolation directory.
///
/// The output is presented as a series of tab-delimited records with the following structure:
///
/// build    fbsource//your:target    local    clang foo.c
///
/// 1: The reason for executing a given command. That's either to build or to test.
///
/// 2: The identity of this command. This will include the target that required it.
///
/// 3: The executor for this command. This will either be RE or local.
///
/// 4: Details to reproduce it. For RE, that's the action digest. For local, the command.
///
/// To reproduce an action that ran on RE, use the following command then follow the instructions.
/// The DIGEST is of the form `hash:size`.
///
/// $ frecli cas download-action DIGEST
///
/// To reproduce an action that ran locally, make sure your working directory is the project root
/// (if unsure, use `buck2 root --kind project` to find it), then run the command. The command is
/// already shell-quoted.
#[derive(Debug, clap::Parser)]
pub struct WhatRanCommand {
    #[clap(flatten)]
    pub common: WhatRanCommandCommon,

    /// Show only commands that failed
    #[clap(long, conflicts_with = "incomplete")]
    pub failed: bool,

    /// Show only commands that were not completed.
    /// That is command were running if buck2 process was killed,
    /// or command currently running if buck2 is running build now.
    #[clap(long)]
    pub incomplete: bool,

    /// Show also std_err from commands that are run.
    /// If the command fails before completing, we display "<command did not finish executing>".
    /// If it finishes but there is no error, we display "<stderr is empty>".
    /// Otherwise, std_err is shown. For JSON, we show raw values and null for non-completion.
    #[clap(long, conflicts_with = "incomplete")]
    pub show_std_err: bool,

    /// Omit commands if their std_err is empty
    #[clap(long, conflicts_with = "incomplete", requires = "show_std_err")]
    pub omit_empty_std_err: bool,
}

#[derive(Debug, clap::Parser)]
pub struct WhatRanCommandCommon {
    #[clap(flatten)]
    event_log: EventLogOptions,

    #[clap(flatten)]
    output: LogCommandOutputFormat,

    #[clap(flatten)]
    options: WhatRanOptions,
}

struct WhatRanCommandOptions {
    options: WhatRanOptions,

    /// Print commands only if they failed.
    failed: bool,

    /// Print commands only if they did not finish.
    incomplete: bool,
}

impl BuckSubcommand for WhatRanCommand {
    const COMMAND_NAME: &'static str = "log-what-ran";

    async fn exec_impl(
        self,
        _matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        _events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let Self {
            common:
                WhatRanCommandCommon {
                    event_log,
                    output,
                    options,
                },
            failed,
            incomplete,
            show_std_err,
            omit_empty_std_err,
        } = self;
        buck2_client_ctx::stdio::print_with_writer::<buck2_error::Error, _>(async move |w| {
            let mut output = OutputFormatWithWriter {
                format: transform_format(output, w),
                include_std_err: show_std_err,
                omit_empty_std_err,
            };
            let log_path = event_log.get(&ctx).await?;

            let (invocation, events) = log_path.unpack_stream().await?;

            buck2_client_ctx::eprintln!(
                "Showing commands from: {}{}",
                invocation.display_command_line(),
                if options.filter_category.is_some() {
                    ", filtered by action category"
                } else {
                    ""
                }
            )?;

            let options = WhatRanCommandOptions {
                options,
                failed,
                incomplete,
            };
            WhatRanCommandState::execute(events, &mut output, &options).await?;
            buck2_error::Ok(())
        })
        .await?;
        ExitResult::success()
    }
}

#[allow(clippy::vec_box)]
struct WhatRanEntry {
    action: WhatRanRelevantAction,
    reproducers: Vec<CommandReproducer>,
    /// Remote stages that begin a submission (queued, or a stage the RE client could not name,
    /// such as a fresh submission's cache check): the digest, and how many reproducers had been
    /// recorded when the stage was reported. An execution stage recorded before such a stage
    /// for the same digest is not shown to belong to the submission that later answered.
    remote_submission_starts: Vec<(String, usize)>,
}

impl WhatRanEntry {
    fn emit_what_ran_entry(
        self,
        output: &mut impl WhatRanOutputWriter,
        options: &WhatRanCommandOptions,
        std_err: Option<&str>,
        duration: Option<std::time::Duration>,
        scheduling_mode: Option<SchedulingMode>,
    ) -> Result<(), ClientIoError> {
        let action = &self.action;
        let options_regex = what_ran::WhatRanOptionsRegex::from_options(&options.options)?;
        for repro in self.reproducers.into_iter() {
            what_ran::emit_what_ran_entry(
                Some(action),
                repro,
                output,
                &options_regex,
                std_err,
                duration,
                scheduling_mode,
            )?;
        }
        Ok(())
    }
}

/// The state for a WhatRan command. This is all the events we have seen that are
/// we have seen that are WhatRanRelevantActions, and the CommandReproducer associated with them.
#[derive(Default)]
pub struct WhatRanCommandState {
    /// Maps action spans to their details, in the order the spans started: unfinished spans are
    /// printed from this map at the end.
    known_actions: BuckMutInsertionMap<SpanId, WhatRanEntry>,
}

impl WhatRanState for WhatRanCommandState {
    fn get(&self, span_id: SpanId) -> Option<WhatRanRelevantAction> {
        self.known_actions.get(&span_id).map(|e| e.action.clone())
    }
}

impl WhatRanCommandState {
    async fn execute(
        mut events: impl Stream<Item = buck2_error::Result<StreamValue>> + Unpin + Send,
        output: &mut impl WhatRanOutputWriter,
        options: &WhatRanCommandOptions,
    ) -> Result<(), ClientIoError> {
        let mut cmd = Self::default();

        while let Some(event) = events.try_next().await? {
            if let StreamValue::Event(event) = event {
                cmd.event(event, output, options)?;
            }
        }

        // emit remaining
        cmd.emit_remaining(output, options)?;
        Ok(())
    }

    fn emit_remaining(
        self,
        output: &mut impl WhatRanOutputWriter,
        options: &WhatRanCommandOptions,
    ) -> buck2_error::Result<()> {
        for (_, entry) in self.known_actions.into_iter() {
            if should_emit_unfinished_action(options) {
                entry.emit_what_ran_entry(output, options, None, None, None)?;
            }
        }
        Ok(())
    }

    /// Receive a new event. We store it if it's relevant and emit them later.
    ///
    /// For each entry we emit we track 4 types of events.
    /// - action start, to create WhatRanRelevantAction
    /// - executor stage, to create CommandReproducer to add to WhatRanRelevantAction
    /// - action end, to emit WhatRanRelevantAction if action finished. If not, action is considered
    ///   unfinished. We check to emit all unfinished after all events are received
    fn event(
        &mut self,
        event: Box<buck2_data::BuckEvent>,
        output: &mut impl WhatRanOutputWriter,
        options: &WhatRanCommandOptions,
    ) -> buck2_error::Result<()> {
        if let Some(data) = event.data {
            // Create WhatRanRelevantAction on SpanStart to track CommandReproducers as they come
            if let Some(action) = WhatRanRelevantAction::from_buck_data(&data) {
                self.known_actions.insert(
                    SpanId::from_u64(event.span_id)?,
                    WhatRanEntry {
                        action,
                        reproducers: Default::default(),
                        remote_submission_starts: Default::default(),
                    },
                );
                return Ok(());
            }
            if let Some(digest) = remote_submission_start(&data) {
                if let Some(parent_id) = SpanId::from_u64_opt(event.parent_id) {
                    if let Some(entry) = self.known_actions.get_mut(&parent_id) {
                        let position = entry.reproducers.len();
                        entry
                            .remote_submission_starts
                            .push((digest.to_owned(), position));
                    }
                }
                return Ok(());
            }
            // Create CommandReproducers on SpanStart an add them to corresponding WhatRanRelevantAction
            if let Some(repro) = CommandReproducer::from_buck_data(&data, &options.options) {
                if let Some(parent_id) = SpanId::from_u64_opt(event.parent_id) {
                    if let Some(entry) = self.known_actions.get_mut(&parent_id) {
                        entry.reproducers.push(repro);
                    }
                }
                return Ok(());
            }
            // Emit WhatRanRelevantAction when we see the corresponding SpanEnd
            if let buck2_data::buck_event::Data::SpanEnd(span) = &data
                && let Some(mut entry) = self
                    .known_actions
                    .shift_remove(&SpanId::from_u64(event.span_id)?)
                && should_emit_finished_action(&span.data, options)
            {
                // Get extra data out of SpanEnd event
                let (execution_kind, std_err, duration, scheduling_mode) =
                    match &span.data {
                        Some(buck2_data::span_end_event::Data::ActionExecution(action_exec)) => (
                            Some(action_exec.execution_kind),
                            action_exec.commands.iter().last().and_then(|cmd| {
                                cmd.details.as_ref().map(|d| d.cmd_stderr.as_ref())
                            }),
                            action_exec.wall_time.as_ref().map(
                                |prost_types::Duration { seconds, nanos }| {
                                    std::time::Duration::new(*seconds as u64, *nanos as u32)
                                },
                            ),
                            action_exec
                                .scheduling_mode
                                .as_ref()
                                .and_then(|o| SchedulingMode::try_from(*o).ok()),
                        ),
                        _ => (None, None, None, None),
                    };

                if execution_kind == Some(buck2_data::ActionExecutionKind::LocalDepFile as i32)
                    && !options.options.skip_cache_hits
                {
                    entry
                        .reproducers
                        .push(CommandReproducer::LocalDepFileCacheHit);
                }
                if let Some((report, remote)) = final_remote_command(&span.data) {
                    apply_remote_origin(
                        &mut entry.reproducers,
                        &entry.remote_submission_starts,
                        report,
                        remote,
                        &options.options,
                    );
                }

                entry.emit_what_ran_entry(output, options, std_err, duration, scheduling_mode)?;
            }
        }

        Ok(())
    }
}

/// The final command report of a finished build action, test run or test discovery, with its
/// remote command if that command was remote.
fn final_remote_command(
    data: &Option<buck2_data::span_end_event::Data>,
) -> Option<(&buck2_data::CommandExecution, &buck2_data::RemoteCommand)> {
    let report = match data.as_ref()? {
        buck2_data::span_end_event::Data::ActionExecution(action_exec) => {
            action_exec.commands.last()?
        }
        buck2_data::span_end_event::Data::TestRun(test_run) => test_run.command_report.as_ref()?,
        buck2_data::span_end_event::Data::TestDiscovery(discovery) => {
            discovery.command_report.as_ref()?
        }
        _ => return None,
    };
    match report
        .details
        .as_ref()?
        .command_kind
        .as_ref()?
        .command
        .as_ref()?
    {
        buck2_data::command_execution_kind::Command::RemoteCommand(remote) => {
            Some((report, remote))
        }
        _ => None,
    }
}

/// The digest of a remote stage that can begin a submission: a queued stage, or a stage the RE
/// client reports without naming it (a fresh submission's cache check is one). Either one after an
/// execution stage leaves it unknown whether that execution belongs to the submission that
/// answered: it may be a new submission, though it may also be the same operation reporting, for
/// example, completion before its response.
fn remote_submission_start(data: &buck2_data::buck_event::Data) -> Option<&str> {
    let buck2_data::buck_event::Data::SpanStart(span) = data else {
        return None;
    };
    let Some(buck2_data::span_start_event::Data::ExecutorStage(stage)) = &span.data else {
        return None;
    };
    let Some(buck2_data::executor_stage_start::Stage::Re(re)) = &stage.stage else {
        return None;
    };
    fn queue_info(info: &Option<buck2_data::ReQueue>) -> Option<&str> {
        info.as_ref().map(|queue| queue.action_digest.as_str())
    }
    match re.stage.as_ref()? {
        buck2_data::re_stage::Stage::Queue(queue) => Some(&queue.action_digest),
        buck2_data::re_stage::Stage::QueueOverQuota(queue) => queue_info(&queue.queue_info),
        buck2_data::re_stage::Stage::QueueNoWorkerAvailable(queue) => queue_info(&queue.queue_info),
        buck2_data::re_stage::Stage::QueueAcquiringDependencies(queue) => {
            queue_info(&queue.queue_info)
        }
        buck2_data::re_stage::Stage::QueueCancelled(queue) => queue_info(&queue.queue_info),
        buck2_data::re_stage::Stage::Unknown(unknown) => Some(&unknown.action_digest),
        _ => None,
    }
}

/// Records how the remote execution service produced the result in `remote`.
///
/// The report describes only the submission that answered. The provider can start the action
/// over after an execution (when the operation is lost, or finished with a transient failure),
/// and the new submission can be answered with or without reporting any stage first; neither the
/// digest nor the absence of a stage between them tells the submissions apart. An execution stage
/// is therefore relabeled with the origin only when it is the last execution stage for the digest,
/// no stage that can begin a submission for the digest follows it, and the submission identity
/// does not contradict it: when both the stage and the report carry the RE client's count of its
/// `Execute` requests, they must be the same request. Logs from writers that did not record the
/// count keep the stage-order rule alone, which cannot detect a restart answered without an
/// intervening stage.
///
/// Otherwise, as when no execution stage was reported at all, every execution stage keeps its own
/// plain label and the answer is recorded from the report alone, so a remote command the service
/// answered is never missing from the output and is never attributed to an attempt it does not
/// describe.
///
/// A report without a service answer (an error or cancellation, which also carry a remote
/// command) adds nothing: nothing shows the service ran or answered it.
fn apply_remote_origin(
    reproducers: &mut Vec<CommandReproducer>,
    remote_submission_starts: &[(String, usize)],
    report: &buck2_data::CommandExecution,
    remote: &buck2_data::RemoteCommand,
    options: &WhatRanOptions,
) {
    match remote.cache_hit_type() {
        // buck2's own cache lookups: reported by their cache-hit stages.
        buck2_data::RemoteCommandOrigin::ActionCache
        | buck2_data::RemoteCommandOrigin::RemoteDepFileCache => return,
        buck2_data::RemoteCommandOrigin::Executed
        | buck2_data::RemoteCommandOrigin::ReServedFromCache
        | buck2_data::RemoteCommandOrigin::ReDeduplicated => {}
    }
    // Any execution stage before the last one for this digest was an attempt whose own outcome
    // the report does not describe.
    let stage = reproducers.iter().rposition(|repro| {
        matches!(repro, CommandReproducer::ReExecute(execute)
            if execute.action_digest == remote.action_digest)
    });
    let answering_stage = stage.filter(|&index| {
        let restarted = remote_submission_starts
            .iter()
            .any(|(digest, position)| *digest == remote.action_digest && *position > index);
        let other_request = match (&reproducers[index], remote.submission) {
            (CommandReproducer::ReExecute(execute), Some(answered)) => execute
                .submission
                .is_some_and(|executed| executed != answered),
            _ => false,
        };
        !restarted && !other_request
    });
    if let Some(index) = answering_stage {
        reproducers[index] = reproducers[index].clone().with_remote_origin(remote);
        return;
    }
    let answered = matches!(
        report.status,
        Some(
            buck2_data::command_execution::Status::Success(..)
                | buck2_data::command_execution::Status::Failure(..)
                | buck2_data::command_execution::Status::Timeout(..)
        )
    );
    if answered {
        reproducers.extend(CommandReproducer::from_terminal_remote_command(
            remote, options,
        ));
    }
}

fn should_emit_finished_action(
    data: &Option<buck2_data::span_end_event::Data>,
    options: &WhatRanCommandOptions,
) -> bool {
    if options.incomplete {
        return false;
    }

    match data {
        Some(buck2_data::span_end_event::Data::ActionExecution(action)) => {
            action.failed || !options.failed
        }
        _ => !options.failed, // This is dead code (this span can only be ActionExecution End given
                              // its ID must match an ActionExecution start).
    }
}

fn should_emit_unfinished_action(options: &WhatRanCommandOptions) -> bool {
    !options.failed // We don't know if it failed or not.
}

/// An output that writes to stdout in a tabulated format.
impl WhatRanOutputWriter for OutputFormatWithWriter<'_> {
    fn emit_command(&mut self, command: WhatRanOutputCommand<'_>) -> buck2_error::Result<()> {
        if self.include_std_err && self.omit_empty_std_err && command.std_err == Some("") {
            return Ok(());
        }
        let std_err_formatted = if self.include_std_err {
            Some(command.std_err.map_or_else(
                || "<command did not finish executing>",
                |std_err| {
                    if std_err.is_empty() {
                        "<std_err is empty>"
                    } else {
                        std_err
                    }
                },
            ))
        } else {
            None
        };

        match &mut self.format {
            LogCommandOutputFormatWithWriter::Readable(w)
            | LogCommandOutputFormatWithWriter::Tabulated(w) => {
                w.write_all(format!("{}\n", command.as_tabulated_reproducer()).as_bytes())?;
                if let Some(std_err) = std_err_formatted {
                    write!(
                        w,
                        "{}{}",
                        std_err,
                        if std_err.ends_with('\n') { "" } else { "\n" }
                    )?;
                }
                Ok(())
            }
            LogCommandOutputFormatWithWriter::Json(w) => {
                let reproducer = match &command.repro {
                    CommandReproducer::CacheQuery(cache_hit) => JsonReproducer::CacheQuery {
                        digest: &cache_hit.action_digest,
                    },
                    CommandReproducer::CacheHit(cache_hit) => match cache_hit.cache_type() {
                        buck2_data::CacheType::ActionCache => JsonReproducer::Cache {
                            digest: &cache_hit.action_digest,
                            action_key: cache_hit.action_key.as_deref(),
                        },
                        buck2_data::CacheType::RemoteDepFileCache => {
                            JsonReproducer::ReDepFileCache {
                                digest: &cache_hit.action_digest,
                                action_key: cache_hit.action_key.as_deref(),
                            }
                        }
                    },
                    CommandReproducer::LocalDepFileCacheHit => JsonReproducer::LocalDepFileCache,
                    CommandReproducer::LocalTestResultCacheQuery(query) => {
                        JsonReproducer::LocalTestResultCacheQuery {
                            digest: &query.action_digest,
                            cache_key: &query.cache_key,
                            current_attempt: &query.current_attempt,
                        }
                    }
                    CommandReproducer::LocalTestResultCache(reused) => {
                        JsonReproducer::LocalTestResultCache {
                            digest: &reused.action_digest,
                            cache_key: &reused.cache_key,
                            origin_attempt: &reused.origin_attempt,
                            entry_manifest: &reused.entry_manifest,
                            current_attempt: &reused.current_attempt,
                        }
                    }
                    CommandReproducer::ReExecute(re_execute)
                    | CommandReproducer::ReServedFromCache(re_execute)
                    | CommandReproducer::ReDeduplicated(re_execute) => {
                        let not_run = match &command.repro {
                            CommandReproducer::ReServedFromCache(..) => Some("served_from_cache"),
                            CommandReproducer::ReDeduplicated(..) => Some("deduplicated"),
                            _ => None,
                        };
                        if re_execute.persistent_worker {
                            JsonReproducer::ReWorker {
                                digest: &re_execute.action_digest,
                                platform_properties: into_index_map(&re_execute.platform),
                                action_key: re_execute.action_key.as_deref(),
                                not_run,
                            }
                        } else {
                            JsonReproducer::Re {
                                digest: &re_execute.action_digest,
                                platform_properties: into_index_map(&re_execute.platform),
                                action_key: re_execute.action_key.as_deref(),
                                not_run,
                            }
                        }
                    }
                    CommandReproducer::LocalExecute(local_execute) => JsonReproducer::Local {
                        command: local_execute.command.as_ref().map_or_else(
                            || Cow::Owned(Vec::new()),
                            |command| Cow::Borrowed(command.argv.as_ref()),
                        ),
                        env: local_execute
                            .command
                            .as_ref()
                            .into_iter()
                            .flat_map(|command| command.env.iter())
                            .map(|entry| (entry.key.as_ref(), entry.value.as_ref()))
                            .collect(),
                    },
                    CommandReproducer::WorkerExecute(worker_execute) => JsonReproducer::Worker {
                        command: worker_execute.command.as_ref().map_or_else(
                            || Cow::Owned(Vec::new()),
                            |command| Cow::Borrowed(command.argv.as_ref()),
                        ),
                        env: worker_execute
                            .command
                            .as_ref()
                            .into_iter()
                            .flat_map(|command| command.env.iter())
                            .map(|entry| (entry.key.as_ref(), entry.value.as_ref()))
                            .collect(),
                    },

                    // TODO(ctolliday): use the worker_id as the `identity`, and add it to worker execution events.
                    // Currently the identity is the first target that used the worker, which might be misleading.
                    CommandReproducer::WorkerInit(worker_init) => JsonReproducer::WorkerInit {
                        command: worker_init.command.as_ref().map_or_else(
                            || Cow::Owned(Vec::new()),
                            |command| Cow::Borrowed(command.argv.as_ref()),
                        ),
                        env: worker_init
                            .command
                            .as_ref()
                            .into_iter()
                            .flat_map(|command| command.env.iter())
                            .map(|entry| (entry.key.as_ref(), entry.value.as_ref()))
                            .collect(),
                    },
                };
                let std_err = if self.include_std_err {
                    Some(command.std_err.unwrap_or("null"))
                } else {
                    None
                };

                let command = JsonCommand {
                    reason: command.reason,
                    identity: command.identity,
                    reproducer,
                    duration: command.duration.map(fmt_duration::fmt_duration),
                    extra: command.extra.map(Into::into),
                    std_err,
                    scheduling_mode: command.scheduling_mode,
                };
                serde_json::to_writer(w.by_ref(), &command)?;
                w.write_all("\n".as_bytes())?;
                Ok(())
            }
            LogCommandOutputFormatWithWriter::Csv(writer) => {
                #[derive(serde::Serialize)]
                struct Record<'a> {
                    reason: &'a str,
                    identity: &'a str,
                    executor: String,
                    reproducer: String,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    std_err: Option<&'a str>,
                }
                writer
                    .serialize(Record {
                        reason: command.reason,
                        identity: command.identity,
                        executor: command.repro.executor(),
                        reproducer: command.repro.to_string(),
                        std_err: std_err_formatted,
                    })
                    .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::LogCmd))?;
                Ok(())
            }
        }
    }
}

fn into_index_map(platform: &Option<buck2_data::RePlatform>) -> BuckMutInsertionMap<&str, &str> {
    platform
        .as_ref()
        .map_or_else(BuckMutInsertionMap::new, |p| {
            p.properties
                .iter()
                .map(|Property { name, value }| (name.as_ref(), value.as_ref()))
                .collect()
        })
}

#[derive(serde::Serialize)]
struct JsonCommand<'a> {
    reason: &'a str,
    identity: &'a str,
    reproducer: JsonReproducer<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<JsonExtra<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    std_err: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduling_mode: Option<SchedulingMode>,
}

mod json_reproducer {
    #![allow(clippy::ref_option_ref)] // within Serialize

    use super::*;

    #[derive(serde::Serialize)]
    #[serde(tag = "executor", content = "details")]
    pub enum JsonReproducer<'a> {
        CacheQuery {
            digest: &'a str,
        },
        Cache {
            digest: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
        },
        ReDepFileCache {
            digest: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
        },
        LocalDepFileCache,
        LocalTestResultCacheQuery {
            digest: &'a str,
            cache_key: &'a str,
            current_attempt: &'a str,
        },
        LocalTestResultCache {
            digest: &'a str,
            cache_key: &'a str,
            origin_attempt: &'a str,
            entry_manifest: &'a str,
            current_attempt: &'a str,
        },
        Re {
            digest: &'a str,
            platform_properties: BuckMutInsertionMap<&'a str, &'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
            /// Why the remote execution service did not run the command, if it did not.
            #[serde(skip_serializing_if = "Option::is_none")]
            not_run: Option<&'static str>,
        },
        ReWorker {
            digest: &'a str,
            platform_properties: BuckMutInsertionMap<&'a str, &'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
            /// Why the remote execution service did not run the command, if it did not.
            #[serde(skip_serializing_if = "Option::is_none")]
            not_run: Option<&'static str>,
        },
        Local {
            command: Cow<'a, [String]>,
            env: BuckMutInsertionMap<&'a str, &'a str>,
        },
        Worker {
            command: Cow<'a, [String]>,
            env: BuckMutInsertionMap<&'a str, &'a str>,
        },
        WorkerInit {
            command: Cow<'a, [String]>,
            env: BuckMutInsertionMap<&'a str, &'a str>,
        },
    }
}

use json_reproducer::JsonReproducer;

#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum JsonExtra<'a> {
    TestCases(&'a [String]),
}

impl<'a> From<WhatRanOutputCommandExtra<'a>> for JsonExtra<'a> {
    fn from(extra: WhatRanOutputCommandExtra<'a>) -> JsonExtra<'a> {
        match extra {
            WhatRanOutputCommandExtra::TestCases(cases) => JsonExtra::TestCases(cases),
        }
    }
}

#[cfg(test)]
mod tests {
    use buck2_event_observer::what_ran::WhatRanOptions;
    use buck2_event_observer::what_ran::WhatRanOutputCommand;
    use buck2_event_observer::what_ran::WhatRanOutputWriter;

    use super::*;
    use crate::what_ran::WhatRanCommandOptions;
    use crate::what_ran::WhatRanCommandState;

    #[test]
    fn local_test_result_reproducer_preserves_original_and_current_provenance()
    -> buck2_error::Result<()> {
        let command = JsonReproducer::LocalTestResultCache {
            digest: "action",
            cache_key: "key",
            origin_attempt: "original",
            entry_manifest: "entry-manifest",
            current_attempt: "current",
        };
        let actual = serde_json::to_value(&command)?;
        assert_eq!(actual["executor"], "LocalTestResultCache");
        assert_eq!(actual["details"]["digest"], "action");
        assert_eq!(actual["details"]["cache_key"], "key");
        assert_eq!(actual["details"]["origin_attempt"], "original");
        assert_eq!(actual["details"]["entry_manifest"], "entry-manifest");
        assert_eq!(actual["details"]["current_attempt"], "current");
        Ok(())
    }

    fn make_base_command() -> JsonCommand<'static> {
        let command = Cow::Owned(vec!["some".to_owned(), "command".to_owned()]);
        let mut env = BuckMutInsertionMap::default();
        env.insert("KEY", "val");

        JsonCommand {
            reason: "test.run",
            identity: "some/target",
            reproducer: JsonReproducer::Local { command, env },
            duration: Some("1".to_owned()),
            extra: None,
            std_err: None,
            scheduling_mode: None,
        }
    }

    fn make_base_command_in_re() -> JsonCommand<'static> {
        JsonCommand {
            reason: "test.run",
            identity: "some/target",
            reproducer: JsonReproducer::Re {
                digest: "placeholder",
                platform_properties: buck2_hash::buck_mut_insertion_map! {
                    "platform" => "linux-remote-execution"
                },
                action_key: None,
                not_run: None,
            },
            duration: Some("1".to_owned()),
            extra: None,
            std_err: None,
            scheduling_mode: None,
        }
    }

    #[test]
    fn serialize_what_ran_command_no_extr() -> buck2_error::Result<()> {
        let command = make_base_command();

        let expected = r#"{
  "reason": "test.run",
  "identity": "some/target",
  "reproducer": {
    "executor": "Local",
    "details": {
      "command": [
        "some",
        "command"
      ],
      "env": {
        "KEY": "val"
      }
    }
  },
  "duration": "1"
}"#;
        assert_eq!(expected, serde_json::to_string_pretty(&command)?);
        Ok(())
    }

    #[test]
    fn serialize_what_ran_command_with_extra() -> buck2_error::Result<()> {
        let cases = ["case".to_owned()];
        let mut command = make_base_command();
        command.extra = Some(JsonExtra::TestCases(&cases));

        let expected = r#"{
  "reason": "test.run",
  "identity": "some/target",
  "reproducer": {
    "executor": "Local",
    "details": {
      "command": [
        "some",
        "command"
      ],
      "env": {
        "KEY": "val"
      }
    }
  },
  "duration": "1",
  "extra": {
    "testcases": [
      "case"
    ]
  }
}"#;
        assert_eq!(expected, serde_json::to_string_pretty(&command)?);
        Ok(())
    }

    #[test]
    fn serialize_what_ran_command_in_re() -> buck2_error::Result<()> {
        let command = make_base_command_in_re();

        let expected = r#"{
  "reason": "test.run",
  "identity": "some/target",
  "reproducer": {
    "executor": "Re",
    "details": {
      "digest": "placeholder",
      "platform_properties": {
        "platform": "linux-remote-execution"
      }
    }
  },
  "duration": "1"
}"#;
        assert_eq!(expected, serde_json::to_string_pretty(&command)?);
        Ok(())
    }

    /// Collects `(identity, executor)` of every emitted command.
    #[derive(Default)]
    struct CollectCommands(Vec<(String, String)>);

    impl WhatRanOutputWriter for CollectCommands {
        fn emit_command(&mut self, command: WhatRanOutputCommand<'_>) -> buck2_error::Result<()> {
            self.0
                .push((command.identity.to_owned(), command.repro.executor()));
            Ok(())
        }
    }

    fn span_event(
        span: u64,
        parent: u64,
        data: buck2_data::buck_event::Data,
    ) -> buck2_data::BuckEvent {
        buck2_data::BuckEvent {
            span_id: span,
            parent_id: parent,
            data: Some(data),
            ..Default::default()
        }
    }

    fn action_start_event() -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
            data: Some(buck2_data::span_start_event::Data::ActionExecution(
                buck2_data::ActionExecutionStart {
                    key: Some(buck2_data::ActionKey {
                        owner: Some(buck2_data::action_key::Owner::TargetLabel(
                            buck2_data::ConfiguredTargetLabel {
                                label: Some(buck2_data::TargetLabel {
                                    package: "root//pkg".to_owned(),
                                    name: "t".to_owned(),
                                }),
                                configuration: Some(buck2_data::Configuration {
                                    full_name: "cfg".to_owned(),
                                }),
                                execution_configuration: None,
                            },
                        )),
                        ..Default::default()
                    }),
                    name: Some(buck2_data::ActionName {
                        category: "cat".to_owned(),
                        identifier: "id".to_owned(),
                    }),
                    kind: buck2_data::ActionKind::Run as i32,
                },
            )),
        })
    }

    fn action_end_event(
        execution_kind: buck2_data::ActionExecutionKind,
    ) -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::SpanEnd(buck2_data::SpanEndEvent {
            data: Some(buck2_data::span_end_event::Data::ActionExecution(Box::new(
                buck2_data::ActionExecutionEnd {
                    kind: buck2_data::ActionKind::Run as i32,
                    execution_kind: execution_kind as i32,
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
    }

    fn test_run_start_event(name: &str) -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
            data: Some(buck2_data::span_start_event::Data::TestRun(
                buck2_data::TestRunStart {
                    suite: Some(buck2_data::TestSuite {
                        suite_name: name.to_owned(),
                        ..Default::default()
                    }),
                },
            )),
        })
    }

    fn local_execute_start_event() -> buck2_data::buck_event::Data {
        buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
            data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                buck2_data::ExecutorStageStart {
                    stage: Some(buck2_data::executor_stage_start::Stage::Local(
                        buck2_data::LocalStage {
                            stage: Some(buck2_data::local_stage::Stage::Execute(
                                buck2_data::LocalExecute::default(),
                            )),
                        },
                    )),
                },
            )),
        })
    }

    fn run_what_ran(
        events: Vec<buck2_data::BuckEvent>,
        options: &WhatRanCommandOptions,
    ) -> buck2_error::Result<Vec<(String, String)>> {
        let mut state = WhatRanCommandState::default();
        let mut out = CollectCommands::default();
        for e in events {
            state.event(Box::new(e), &mut out, options)?;
        }
        state.emit_remaining(&mut out, options)?;
        Ok(out.0)
    }

    /// A local dep-file cache hit is a cache hit, so `--skip-cache-hits` drops it.
    #[test]
    fn test_skip_cache_hits_skips_local_dep_file_hits() -> buck2_error::Result<()> {
        let events = || {
            vec![
                span_event(1, 0, action_start_event()),
                span_event(
                    1,
                    0,
                    action_end_event(buck2_data::ActionExecutionKind::LocalDepFile),
                ),
            ]
        };
        let executors = |options: &WhatRanCommandOptions| -> buck2_error::Result<Vec<String>> {
            Ok(run_what_ran(events(), options)?
                .into_iter()
                .map(|(_, e)| e)
                .collect())
        };

        let options = WhatRanCommandOptions {
            options: WhatRanOptions::default(),
            failed: false,
            incomplete: false,
        };
        assert_eq!(vec!["dep_file".to_owned()], executors(&options)?);

        let options = WhatRanCommandOptions {
            options: WhatRanOptions {
                skip_cache_hits: true,
                ..Default::default()
            },
            failed: false,
            incomplete: false,
        };
        assert_eq!(Vec::<String>::new(), executors(&options)?);
        Ok(())
    }

    /// Commands of spans that never ended are printed in the order the spans started.
    #[test]
    fn test_incomplete_commands_in_start_order() -> buck2_error::Result<()> {
        let options = WhatRanCommandOptions {
            options: WhatRanOptions::default(),
            failed: false,
            incomplete: true,
        };
        let mut events = Vec::new();
        for i in 0..64u64 {
            let span = 2 * i + 1;
            events.push(span_event(span, 0, test_run_start_event(&format!("s{i}"))));
            events.push(span_event(span + 1, span, local_execute_start_event()));
        }
        let identities: Vec<String> = run_what_ran(events, &options)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let start_order: Vec<String> = (0..64).map(|i| format!("s{i}")).collect();

        assert_eq!(start_order, identities);
        Ok(())
    }

    /// Remote outcomes of build actions, test runs and test discoveries, through the real
    /// `WhatRanCommandState::event` boundary and the real tabulated and JSON writers.
    mod remote_origin {
        use super::*;

        const DIGEST: &str = "0123456789abcdef:42";

        #[derive(Clone, Copy, Debug)]
        enum Kind {
            Build,
            TestRun,
            TestDiscovery,
        }

        const KINDS: [Kind; 3] = [Kind::Build, Kind::TestRun, Kind::TestDiscovery];

        const ORIGINS: [(buck2_data::RemoteCommandOrigin, &str, Option<&str>); 3] = [
            (buck2_data::RemoteCommandOrigin::Executed, "re", None),
            (
                buck2_data::RemoteCommandOrigin::ReServedFromCache,
                "re_served_from_cache",
                Some("served_from_cache"),
            ),
            (
                buck2_data::RemoteCommandOrigin::ReDeduplicated,
                "re_deduplicated",
                Some("deduplicated"),
            ),
        ];

        fn options() -> WhatRanCommandOptions {
            WhatRanCommandOptions {
                options: WhatRanOptions::default(),
                failed: false,
                incomplete: false,
            }
        }

        fn platform() -> buck2_data::RePlatform {
            buck2_data::RePlatform {
                properties: vec![buck2_data::re_platform::Property {
                    name: "OSFamily".to_owned(),
                    value: "linux".to_owned(),
                }],
            }
        }

        /// The execution stage the RE client reports once the service says it is executing.
        fn re_execute_stage_start() -> buck2_data::buck_event::Data {
            buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                    buck2_data::ExecutorStageStart {
                        stage: Some(buck2_data::executor_stage_start::Stage::Re(
                            buck2_data::ReStage {
                                stage: Some(buck2_data::re_stage::Stage::Execute(
                                    buck2_data::ReExecute {
                                        action_digest: DIGEST.to_owned(),
                                        platform: Some(platform()),
                                        action_key: None,
                                        use_case: "buck2-default".to_owned(),
                                        persistent_worker: false,
                                        submission: None,
                                    },
                                )),
                            },
                        )),
                    },
                )),
            })
        }

        /// The stage the RE client reports while the submission is queued: it never yields a
        /// reproducer.
        fn re_queue_stage_start() -> buck2_data::buck_event::Data {
            buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                    buck2_data::ExecutorStageStart {
                        stage: Some(buck2_data::executor_stage_start::Stage::Re(
                            buck2_data::ReStage {
                                stage: Some(buck2_data::re_stage::Stage::Queue(
                                    buck2_data::ReQueue {
                                        action_digest: DIGEST.to_owned(),
                                        use_case: "buck2-default".to_owned(),
                                    },
                                )),
                            },
                        )),
                    },
                )),
            })
        }

        fn remote_report(
            origin: buck2_data::RemoteCommandOrigin,
            status: buck2_data::command_execution::Status,
        ) -> buck2_data::CommandExecution {
            buck2_data::CommandExecution {
                details: Some(buck2_data::CommandExecutionDetails {
                    command_kind: Some(buck2_data::CommandExecutionKind {
                        command: Some(buck2_data::command_execution_kind::Command::RemoteCommand(
                            buck2_data::RemoteCommand {
                                action_digest: DIGEST.to_owned(),
                                cache_hit: matches!(
                                    origin,
                                    buck2_data::RemoteCommandOrigin::ActionCache
                                        | buck2_data::RemoteCommandOrigin::RemoteDepFileCache
                                ),
                                cache_hit_type: origin.into(),
                                details: Some(buck2_data::RemoteCommandDetails {
                                    session_id: None,
                                    use_case: "buck2-default".to_owned(),
                                    platform: Some(platform()),
                                    persistent_worker: false,
                                }),
                                ..Default::default()
                            },
                        )),
                    }),
                    ..Default::default()
                }),
                status: Some(status),
                ..Default::default()
            }
        }

        fn success() -> buck2_data::command_execution::Status {
            buck2_data::command_execution::Success {}.into()
        }

        fn start_event(kind: Kind) -> buck2_data::buck_event::Data {
            match kind {
                Kind::Build => action_start_event(),
                Kind::TestRun => test_run_start_event("suite"),
                Kind::TestDiscovery => {
                    buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                        data: Some(buck2_data::span_start_event::Data::TestDiscovery(
                            buck2_data::TestDiscoveryStart {
                                suite_name: "discovered".to_owned(),
                                target_label: None,
                            },
                        )),
                    })
                }
            }
        }

        fn end_event(
            kind: Kind,
            report: buck2_data::CommandExecution,
        ) -> buck2_data::buck_event::Data {
            let data = match kind {
                Kind::Build => buck2_data::span_end_event::Data::ActionExecution(Box::new(
                    buck2_data::ActionExecutionEnd {
                        kind: buck2_data::ActionKind::Run as i32,
                        commands: vec![report],
                        ..Default::default()
                    },
                )),
                Kind::TestRun => buck2_data::TestRunEnd {
                    suite: Some(buck2_data::TestSuite {
                        suite_name: "suite".to_owned(),
                        ..Default::default()
                    }),
                    command_report: Some(report),
                    command_host_sharing_requirements: None,
                }
                .into(),
                Kind::TestDiscovery => buck2_data::TestDiscoveryEnd {
                    suite_name: "discovered".to_owned(),
                    command_report: Some(report),
                    ..Default::default()
                }
                .into(),
            };
            buck2_data::buck_event::Data::SpanEnd(buck2_data::SpanEndEvent {
                data: Some(data),
                ..Default::default()
            })
        }

        fn reason_and_identity(kind: Kind) -> (&'static str, String) {
            match kind {
                Kind::Build => ("build", build_identity()),
                Kind::TestRun => ("test.run", "suite".to_owned()),
                Kind::TestDiscovery => ("test.discovery", "discovered".to_owned()),
            }
        }

        /// How the build action of `action_start_event` is identified.
        fn build_identity() -> String {
            match action_start_event() {
                buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                    data: Some(buck2_data::span_start_event::Data::ActionExecution(start)),
                }) => buck2_event_observer::display::display_action_identity(
                    start.key.as_ref(),
                    start.name.as_ref(),
                    buck2_event_observer::display::TargetDisplayOptions::for_log(),
                )
                .unwrap(),
                _ => unreachable!(),
            }
        }

        /// One action span (id 1) with the given stage events under it, ended by `report`.
        fn action(
            kind: Kind,
            stages: Vec<buck2_data::buck_event::Data>,
            report: buck2_data::CommandExecution,
        ) -> Vec<buck2_data::BuckEvent> {
            let mut events = vec![span_event(1, 0, start_event(kind))];
            for (i, stage) in stages.into_iter().enumerate() {
                events.push(span_event(10 + i as u64, 1, stage));
            }
            events.push(span_event(1, 0, end_event(kind, report)));
            events
        }

        /// Runs the events through `WhatRanCommandState::event` into the real writer.
        fn render(
            events: &[buck2_data::BuckEvent],
            options: &WhatRanCommandOptions,
            json: bool,
        ) -> String {
            let mut buf = Vec::new();
            {
                let mut out = OutputFormatWithWriter {
                    format: if json {
                        LogCommandOutputFormatWithWriter::Json(&mut buf)
                    } else {
                        LogCommandOutputFormatWithWriter::Tabulated(&mut buf)
                    },
                    include_std_err: false,
                    omit_empty_std_err: false,
                };
                let mut state = WhatRanCommandState::default();
                for e in events {
                    state.event(Box::new(e.clone()), &mut out, options).unwrap();
                }
                state.emit_remaining(&mut out, options).unwrap();
            }
            String::from_utf8(buf).unwrap()
        }

        /// Asserts the events produce exactly one remote record for `kind`, with the given
        /// executor label and `not_run` provenance, and no duration (none was reported, and
        /// none may be invented).
        fn assert_one_remote_record(
            events: &[buck2_data::BuckEvent],
            kind: Kind,
            executor: &str,
            not_run: Option<&str>,
        ) {
            let (reason, identity) = reason_and_identity(kind);
            let context = format!("{kind:?} {executor}");
            assert_eq!(
                render(events, &options(), false),
                format!("{reason}\t{identity}\t{executor}(OSFamily=linux)\t{DIGEST}\n"),
                "{context}"
            );
            let json = render(events, &options(), true);
            let records: Vec<serde_json::Value> = json
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(records.len(), 1, "{context}: {json}");
            let record = &records[0];
            assert_eq!(record["reason"], reason, "{context}");
            assert_eq!(record["identity"], identity.as_str(), "{context}");
            assert_eq!(record["reproducer"]["executor"], "Re", "{context}");
            let details = &record["reproducer"]["details"];
            assert_eq!(details["digest"], DIGEST, "{context}");
            assert_eq!(
                details["platform_properties"]["OSFamily"], "linux",
                "{context}"
            );
            assert_eq!(
                details.get("not_run").and_then(|v| v.as_str()),
                not_run,
                "{context}"
            );
            assert!(record.get("duration").is_none(), "{context}: {json}");
        }

        #[test]
        fn remote_outcome_with_execution_stage_keeps_identity_and_origin() {
            for kind in KINDS {
                for (origin, executor, not_run) in ORIGINS {
                    let events = action(
                        kind,
                        vec![re_queue_stage_start(), re_execute_stage_start()],
                        remote_report(origin, success()),
                    );
                    assert_one_remote_record(&events, kind, executor, not_run);
                }
            }
        }

        /// The service answered without ever reporting that it was executing, so the RE client
        /// emitted no execution stage: the terminal report alone must still produce the record.
        #[test]
        fn terminal_only_remote_outcome_is_recorded_with_identity_and_origin() {
            for kind in KINDS {
                for (origin, executor, not_run) in ORIGINS {
                    for stages in [vec![], vec![re_queue_stage_start()]] {
                        let events = action(kind, stages, remote_report(origin, success()));
                        assert_one_remote_record(&events, kind, executor, not_run);
                    }
                    // A failing or timed-out command was also answered by the service.
                    for status in [
                        buck2_data::command_execution::Failure::default().into(),
                        buck2_data::command_execution::Timeout::default().into(),
                    ] {
                        let events = action(kind, vec![], remote_report(origin, status));
                        assert_one_remote_record(&events, kind, executor, not_run);
                    }
                }
            }
        }

        /// Only the final submission produced the reported result; an earlier attempt keeps its
        /// plain execution label.
        #[test]
        fn only_the_last_execution_stage_takes_the_reported_origin() {
            for kind in KINDS {
                let events = action(
                    kind,
                    vec![re_execute_stage_start(), re_execute_stage_start()],
                    remote_report(
                        buck2_data::RemoteCommandOrigin::ReServedFromCache,
                        success(),
                    ),
                );
                let (reason, identity) = reason_and_identity(kind);
                assert_eq!(
                    render(&events, &options(), false),
                    format!(
                        "{reason}\t{identity}\tre(OSFamily=linux)\t{DIGEST}\n\
                         {reason}\t{identity}\tre_served_from_cache(OSFamily=linux)\t{DIGEST}\n"
                    ),
                    "{kind:?}"
                );
            }
        }

        /// The stage the RE client reports for a provider stage it has no specific stage for,
        /// such as a fresh submission's `CACHE_CHECK`.
        fn re_unknown_stage_start() -> buck2_data::buck_event::Data {
            buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                    buck2_data::ExecutorStageStart {
                        stage: Some(buck2_data::executor_stage_start::Stage::Re(
                            buck2_data::ReStage {
                                stage: Some(buck2_data::re_stage::Stage::Unknown(
                                    buck2_data::ReUnknown {
                                        action_digest: DIGEST.to_owned(),
                                    },
                                )),
                            },
                        )),
                    },
                )),
            })
        }

        /// S1-EVENT-03: the provider started the action over after an execution (the operation
        /// was lost, or the server failed it transiently), and the new submission was answered
        /// without reporting that it executed. The earlier execution keeps its plain label and
        /// the answer is recorded separately from the report, rather than relabeling the earlier
        /// execution with an origin that describes a different submission.
        #[test]
        fn restarted_submission_answered_without_execution_keeps_the_earlier_execution() {
            for kind in KINDS {
                for (origin, executor, not_run) in ORIGINS {
                    for restart in [re_queue_stage_start(), re_unknown_stage_start()] {
                        let events = action(
                            kind,
                            vec![re_queue_stage_start(), re_execute_stage_start(), restart],
                            remote_report(origin, success()),
                        );
                        let (reason, identity) = reason_and_identity(kind);
                        assert_eq!(
                            render(&events, &options(), false),
                            format!(
                                "{reason}\t{identity}\tre(OSFamily=linux)\t{DIGEST}\n\
                                 {reason}\t{identity}\t{executor}(OSFamily=linux)\t{DIGEST}\n"
                            ),
                            "{kind:?} {executor}"
                        );
                        let json = render(&events, &options(), true);
                        let not_runs: Vec<Option<String>> = json
                            .lines()
                            .map(|l| {
                                let record: serde_json::Value = serde_json::from_str(l).unwrap();
                                record["reproducer"]["details"]
                                    .get("not_run")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned)
                            })
                            .collect();
                        assert_eq!(
                            not_runs,
                            vec![None, not_run.map(str::to_owned)],
                            "{kind:?} {executor}: {json}"
                        );
                    }
                }

                // Controls: the restart never invents a record under `--skip-remote-executions`,
                // or for a service that never answered.
                let restarted = |status| {
                    action(
                        kind,
                        vec![
                            re_queue_stage_start(),
                            re_execute_stage_start(),
                            re_queue_stage_start(),
                        ],
                        remote_report(buck2_data::RemoteCommandOrigin::ReServedFromCache, status),
                    )
                };
                let skip_remote = WhatRanCommandOptions {
                    options: WhatRanOptions {
                        skip_remote_executions: true,
                        ..Default::default()
                    },
                    failed: false,
                    incomplete: false,
                };
                assert_eq!(
                    render(&restarted(success()), &skip_remote, false),
                    "",
                    "{kind:?}"
                );
                for status in [
                    buck2_data::command_execution::Cancelled {}.into(),
                    buck2_data::command_execution::Error::default().into(),
                ] {
                    let (reason, identity) = reason_and_identity(kind);
                    assert_eq!(
                        render(&restarted(status), &options(), false),
                        format!("{reason}\t{identity}\tre(OSFamily=linux)\t{DIGEST}\n"),
                        "{kind:?}"
                    );
                }
            }
        }

        /// An execution stage recording the RE client's request count, `None` as in a log from a
        /// writer that did not record it.
        fn execute_stage(submission: Option<u32>) -> buck2_data::buck_event::Data {
            let mut stage = re_execute_stage_start();
            if let buck2_data::buck_event::Data::SpanStart(start) = &mut stage
                && let Some(buck2_data::span_start_event::Data::ExecutorStage(stage)) =
                    &mut start.data
                && let Some(buck2_data::executor_stage_start::Stage::Re(re)) = &mut stage.stage
                && let Some(buck2_data::re_stage::Stage::Execute(execute)) = &mut re.stage
            {
                execute.submission = submission;
            }
            stage
        }

        /// A remote report recording which request the service answered, `None` as in a log from
        /// a writer that did not record it.
        fn report_for(
            origin: buck2_data::RemoteCommandOrigin,
            status: buck2_data::command_execution::Status,
            submission: Option<u32>,
        ) -> buck2_data::CommandExecution {
            let mut report = remote_report(origin, status);
            if let Some(buck2_data::command_execution_kind::Command::RemoteCommand(remote)) =
                &mut report
                    .details
                    .as_mut()
                    .unwrap()
                    .command_kind
                    .as_mut()
                    .unwrap()
                    .command
            {
                remote.submission = submission;
            }
            report
        }

        /// The new fork-only fields (10001) through the real log reader and `what-ran`'s own
        /// stream path, in protobuf and JSON logs, with and without the `muninn_event_schema`
        /// marker. A log that records the request count separates a restart answered without any
        /// stage; a log without it (every log written before the count existed, marked or
        /// unmarked) reads unchanged under the EVENT03 stage-order rule: it still separates a
        /// restart that reported a stage, and cannot detect one that did not.
        #[tokio::test]
        async fn request_count_round_trips_and_old_logs_keep_the_stage_rule() {
            use buck2_event_log::read::EventLogPathBuf;
            use buck2_fs::paths::abs_path::AbsPathBuf;
            use prost::Message;

            let served = buck2_data::RemoteCommandOrigin::ReServedFromCache;
            let restart = |stages: Vec<buck2_data::buck_event::Data>, answered: Option<u32>| {
                action(
                    Kind::TestRun,
                    stages,
                    report_for(served, success(), answered),
                )
            };
            let line =
                |executor: &str| format!("test.run\tsuite\t{executor}(OSFamily=linux)\t{DIGEST}\n");
            let cases: Vec<(&str, Vec<buck2_data::BuckEvent>, String)> = vec![
                (
                    "count recorded, another request answered directly",
                    restart(
                        vec![re_queue_stage_start(), execute_stage(Some(1))],
                        Some(2),
                    ),
                    line("re") + &line("re_served_from_cache"),
                ),
                (
                    "count recorded, the same request answered",
                    restart(
                        vec![re_queue_stage_start(), execute_stage(Some(1))],
                        Some(1),
                    ),
                    line("re_served_from_cache"),
                ),
                (
                    "no count, restart reported a queued stage",
                    restart(
                        vec![
                            re_queue_stage_start(),
                            execute_stage(None),
                            re_queue_stage_start(),
                        ],
                        None,
                    ),
                    line("re") + &line("re_served_from_cache"),
                ),
                (
                    "no count, no stage after the execution",
                    restart(vec![re_queue_stage_start(), execute_stage(None)], None),
                    line("re_served_from_cache"),
                ),
            ];

            let dir = tempfile::TempDir::new().unwrap();
            for (case, events, expected) in cases {
                for marker in [Some(2), None] {
                    for json in [false, true] {
                        let context = format!("{case}, marker {marker:?}, json {json}");
                        let invocation = buck2_data::Invocation {
                            command_line_args: vec!["buck2".to_owned(), "test".to_owned()],
                            expanded_command_line_args: vec![],
                            working_dir: "/".to_owned(),
                            trace_id: Some(
                                buck2_wrapper_common::invocation_id::TraceId::null().to_string(),
                            ),
                            start_time: None,
                            muninn_event_schema: marker,
                        };
                        let name = format!(
                            "{}-{}.{}",
                            case.replace([' ', ','], "-"),
                            marker.is_some(),
                            if json { "json-lines" } else { "pb" }
                        );
                        let path = dir.path().join(name);
                        if json {
                            let mut header = serde_json::to_value(&invocation).unwrap();
                            if marker.is_none() {
                                header
                                    .as_object_mut()
                                    .unwrap()
                                    .remove("muninn_event_schema");
                            }
                            let mut text = format!("{header}\n");
                            for event in &events {
                                text.push_str(&format!(
                                    "{}\n",
                                    serde_json::json!({ "Event": event })
                                ));
                            }
                            std::fs::write(&path, text).unwrap();
                        } else {
                            let mut raw = Vec::new();
                            invocation.encode_length_delimited(&mut raw).unwrap();
                            for event in &events {
                                buck2_cli_proto::CommandProgress {
                                    progress: Some(
                                        buck2_cli_proto::command_progress::Progress::Event(
                                            Box::new(event.clone()),
                                        ),
                                    ),
                                }
                                .encode_length_delimited(&mut raw)
                                .unwrap();
                            }
                            std::fs::write(&path, raw).unwrap();
                        }
                        let log = EventLogPathBuf::infer(AbsPathBuf::new(path).unwrap()).unwrap();
                        let (_, stream) = log.unpack_stream().await.unwrap();
                        let mut buf = Vec::new();
                        {
                            let mut out = OutputFormatWithWriter {
                                format: LogCommandOutputFormatWithWriter::Tabulated(&mut buf),
                                include_std_err: false,
                                omit_empty_std_err: false,
                            };
                            WhatRanCommandState::execute(Box::pin(stream), &mut out, &options())
                                .await
                                .unwrap();
                        }
                        assert_eq!(String::from_utf8(buf).unwrap(), expected, "{context}");
                    }
                }
            }
        }

        /// S1-EVENT-04: when the stage and the report both carry the RE client's request count, a
        /// different request is never relabeled, even with no stage between them; the same request
        /// is. A legacy stage without the count keeps the stage-order rule, and a stage that can
        /// begin a submission still keeps the conservative split for the same request.
        #[test]
        fn submission_identity_decides_which_execution_answered() {
            let execute = execute_stage;
            let report = |origin, submission| report_for(origin, success(), submission);
            for kind in KINDS {
                let (reason, identity) = reason_and_identity(kind);
                let line = |executor: &str| {
                    format!("{reason}\t{identity}\t{executor}(OSFamily=linux)\t{DIGEST}\n")
                };
                for (origin, executor, _) in ORIGINS {
                    let context = format!("{kind:?} {executor}");
                    // Another request answered, with no stage between: both records.
                    let events = action(
                        kind,
                        vec![re_queue_stage_start(), execute(Some(1))],
                        report(origin, Some(2)),
                    );
                    assert_eq!(
                        render(&events, &options(), false),
                        line("re") + &line(executor),
                        "{context}"
                    );
                    // The same request answered: one record with the service's origin.
                    let events = action(
                        kind,
                        vec![re_queue_stage_start(), execute(Some(1))],
                        report(origin, Some(1)),
                    );
                    assert_one_remote_record(
                        &events,
                        kind,
                        executor,
                        ORIGINS
                            .iter()
                            .find(|(_, e, _)| *e == executor)
                            .and_then(|(_, _, not_run)| *not_run),
                    );
                    // A later execution of the answering request takes the origin; the earlier
                    // request keeps its plain label.
                    let events = action(
                        kind,
                        vec![execute(Some(1)), execute(Some(2))],
                        report(origin, Some(2)),
                    );
                    assert_eq!(
                        render(&events, &options(), false),
                        line("re") + &line(executor),
                        "{context}"
                    );
                    // The same request, but a stage that can begin a submission follows its
                    // execution: still shown separately.
                    let events = action(
                        kind,
                        vec![
                            re_queue_stage_start(),
                            execute(Some(1)),
                            re_unknown_stage_start(),
                        ],
                        report(origin, Some(1)),
                    );
                    assert_eq!(
                        render(&events, &options(), false),
                        line("re") + &line(executor),
                        "{context}"
                    );
                    // Legacy logs record no request on either side, or on one side only: the
                    // stage-order rule alone decides.
                    for (stage_submission, report_submission) in
                        [(None, None), (None, Some(2)), (Some(1), None)]
                    {
                        let events = action(
                            kind,
                            vec![re_queue_stage_start(), execute(stage_submission)],
                            report(origin, report_submission),
                        );
                        assert_eq!(
                            render(&events, &options(), false),
                            line(executor),
                            "{context} {stage_submission:?} {report_submission:?}"
                        );
                    }
                }
            }
        }

        /// A queue stage for a different action does not separate this action's execution from
        /// its answer.
        #[test]
        fn another_digests_queue_stage_does_not_separate_the_execution_from_its_answer() {
            let other_queue = buck2_data::buck_event::Data::SpanStart(buck2_data::SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                    buck2_data::ExecutorStageStart {
                        stage: Some(buck2_data::executor_stage_start::Stage::Re(
                            buck2_data::ReStage {
                                stage: Some(buck2_data::re_stage::Stage::Queue(
                                    buck2_data::ReQueue {
                                        action_digest: "fedcba9876543210:7".to_owned(),
                                        use_case: "buck2-default".to_owned(),
                                    },
                                )),
                            },
                        )),
                    },
                )),
            });
            for kind in KINDS {
                let events = action(
                    kind,
                    vec![
                        re_queue_stage_start(),
                        re_execute_stage_start(),
                        other_queue.clone(),
                    ],
                    remote_report(
                        buck2_data::RemoteCommandOrigin::ReServedFromCache,
                        success(),
                    ),
                );
                assert_one_remote_record(
                    &events,
                    kind,
                    "re_served_from_cache",
                    Some("served_from_cache"),
                );
            }
        }

        /// Controls: nothing is invented for remote reports the service never answered, for
        /// buck2's own cache hits, under `--skip-remote-executions`, or for local test reuse.
        #[test]
        fn no_remote_record_without_a_service_answer_or_when_filtered() {
            for kind in KINDS {
                for status in [
                    buck2_data::command_execution::Cancelled {}.into(),
                    buck2_data::command_execution::Error::default().into(),
                ] {
                    let events = action(
                        kind,
                        vec![re_queue_stage_start()],
                        remote_report(buck2_data::RemoteCommandOrigin::Executed, status),
                    );
                    assert_eq!(render(&events, &options(), false), "", "{kind:?}");
                }

                let served = action(
                    kind,
                    vec![],
                    remote_report(
                        buck2_data::RemoteCommandOrigin::ReServedFromCache,
                        success(),
                    ),
                );
                let skip_remote = WhatRanCommandOptions {
                    options: WhatRanOptions {
                        skip_remote_executions: true,
                        ..Default::default()
                    },
                    failed: false,
                    incomplete: false,
                };
                assert_eq!(render(&served, &skip_remote, false), "", "{kind:?}");
            }

            // buck2's own action-cache hit is reported once, by its cache-hit stage.
            let cache_hit = action(
                Kind::Build,
                vec![buck2_data::buck_event::Data::SpanStart(
                    buck2_data::SpanStartEvent {
                        data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                            buck2_data::ExecutorStageStart {
                                stage: Some(buck2_data::executor_stage_start::Stage::CacheHit(
                                    buck2_data::CacheHit {
                                        action_digest: DIGEST.to_owned(),
                                        ..Default::default()
                                    },
                                )),
                            },
                        )),
                    },
                )],
                remote_report(buck2_data::RemoteCommandOrigin::ActionCache, success()),
            );
            assert_eq!(
                run_what_ran(cache_hit, &options()).unwrap(),
                vec![(build_identity(), "cache".to_owned())]
            );

            // A local test-result reuse stays a local reuse.
            let reused = buck2_data::LocalTestResultCache {
                action_digest: DIGEST.to_owned(),
                cache_key: "key".to_owned(),
                origin_attempt: "origin".to_owned(),
                entry_manifest: "manifest".to_owned(),
                current_attempt: "current".to_owned(),
            };
            let mut report = remote_report(buck2_data::RemoteCommandOrigin::Executed, success());
            report.details.as_mut().unwrap().command_kind =
                Some(buck2_data::CommandExecutionKind {
                    command: Some(
                        buck2_data::command_execution_kind::Command::LocalTestResultCache(
                            reused.clone(),
                        ),
                    ),
                });
            let reuse = action(
                Kind::TestRun,
                vec![buck2_data::buck_event::Data::SpanStart(
                    buck2_data::SpanStartEvent {
                        data: Some(buck2_data::span_start_event::Data::ExecutorStage(
                            buck2_data::ExecutorStageStart {
                                stage: Some(
                                    buck2_data::executor_stage_start::Stage::LocalTestResultCache(
                                        reused,
                                    ),
                                ),
                            },
                        )),
                    },
                )],
                report,
            );
            assert_eq!(
                run_what_ran(reuse, &options()).unwrap(),
                vec![("suite".to_owned(), "local_test_result_cache".to_owned())]
            );
        }

        /// Joins the terminal-only case to the real producers: the OSS provider decodes a stream
        /// that holds only the finished operation (no progress update, as a server answering
        /// from its cache may send), the engine derives the origin from that response and
        /// renders the command report from it, and what-ran consumes the resulting test run.
        #[cfg(not(fbcode_build))]
        #[tokio::test]
        async fn provider_terminal_only_cached_answer_reaches_what_ran() {
            use buck2_execute::execute::action_digest::ActionDigest;
            use buck2_execute::execute::kind::RemoteCommandExecutionDetails;
            use buck2_execute::execute::kind::RemoteExecutionOrigin;
            use buck2_execute::re::remote_action_result::ExecuteResponseWithQueueStats;
            use buck2_execute::re::remote_action_result::RemoteActionResult;
            use futures::StreamExt;
            use prost::Message;
            use re_grpc_proto::build::bazel::remote::execution::v2 as reapi;
            use re_grpc_proto::google::longrunning::Operation;
            use re_grpc_proto::google::longrunning::operation::Result as OpResult;

            let finished = Operation {
                name: "operations/served".to_owned(),
                done: true,
                result: Some(OpResult::Response(prost_types::Any {
                    type_url: String::new(),
                    value: reapi::ExecuteResponse {
                        result: Some(reapi::ActionResult {
                            execution_metadata: Some(reapi::ExecutedActionMetadata::default()),
                            ..Default::default()
                        }),
                        cached_result: true,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                })),
                ..Default::default()
            };
            let progress: Vec<_> = remote_execution::execution_progress_stream(
                move |_resume: Option<String>| {
                    let finished = finished.clone();
                    async move {
                        anyhow::Ok(futures::stream::iter(vec![Ok::<_, tonic::Status>(
                            finished,
                        )]))
                    }
                },
                0,
            )
            .collect()
            .await;
            let progress: Vec<_> = progress.into_iter().map(|p| p.unwrap()).collect();
            // No EXECUTING update: the RE client emits its execution stage only for that one.
            assert_eq!(
                progress.iter().map(|p| p.stage).collect::<Vec<_>>(),
                vec![remote_execution::Stage::COMPLETED]
            );
            let execute_response = progress
                .into_iter()
                .next()
                .and_then(|p| p.execute_response)
                .unwrap();

            let response = ExecuteResponseWithQueueStats {
                execute_response,
                queue_stats: Default::default(),
                submission: None,
            };
            assert_eq!(response.origin(), RemoteExecutionOrigin::ServedFromCache);
            let action_digest = ActionDigest::new_sha1([7; 20], 42);
            let execution_kind = response.execution_kind(RemoteCommandExecutionDetails::new(
                action_digest.clone(),
                None,
                None,
                buck2_core::execution_types::executor_config::RemoteExecutorUseCase::new(
                    "buck2-default".to_owned(),
                ),
                &remote_execution::Platform {
                    properties: vec![reapi::platform::Property {
                        name: "OSFamily".to_owned(),
                        value: "linux".to_owned(),
                    }],
                },
                false,
            ));
            let report = buck2_data::CommandExecution {
                details: Some(buck2_data::CommandExecutionDetails {
                    command_kind: Some(execution_kind.to_proto(false)),
                    ..Default::default()
                }),
                status: Some(success()),
                ..Default::default()
            };

            let events = action(Kind::TestRun, vec![re_queue_stage_start()], report);
            let digest = action_digest.to_string();
            assert_eq!(
                render(&events, &options(), false),
                format!("test.run\tsuite\tre_served_from_cache(OSFamily=linux)\t{digest}\n")
            );
        }

        /// Joins what-ran to the real producers: the OSS provider's progress stream, driven by a
        /// scripted server, feeds the engine RE client's actual stage producer inside a real
        /// test-run span on a real dispatcher; the engine derives the command report from the
        /// answering response, and what-ran reads the emitted events. Only the gRPC calls are
        /// scripted (and the liveliness observer never fires).
        #[cfg(not(fbcode_build))]
        mod real_producer {
            use std::collections::VecDeque;
            use std::sync::Arc;
            use std::sync::Mutex;

            use buck2_build_signals::env::WaitingData;
            use buck2_common::liveliness_observer::NoopLivelinessObserver;
            use buck2_events::dispatch::EventDispatcher;
            use buck2_events::dispatch::span_async;
            use buck2_events::dispatch::with_dispatcher_async;
            use buck2_execute::execute::action_digest::ActionDigest;
            use buck2_execute::execute::claim::MutexClaimManager;
            use buck2_execute::execute::kind::RemoteCommandExecutionDetails;
            use buck2_execute::execute::manager::CommandExecutionManager;
            use buck2_execute::knobs::ExecutorGlobalKnobs;
            use buck2_execute::re::client::ExecuteResponseOrCancelled;
            use buck2_execute::re::client::report_execution_progress;
            use buck2_execute::re::remote_action_result::RemoteActionResult;
            use futures::StreamExt;
            use prost::Message;
            use re_grpc_proto::build::bazel::remote::execution::v2 as reapi;
            use re_grpc_proto::google::longrunning::Operation;
            use re_grpc_proto::google::longrunning::operation::Result as OpResult;
            use reapi::execution_stage::Value as ServerStage;

            use super::*;

            type Message_ = Result<Operation, tonic::Status>;
            /// One call to the provider's opener: a stream of messages, or a failed call.
            type Call = Result<Vec<Message_>, tonic::Status>;

            const A: &str = "operations/a";
            const B: &str = "operations/b";

            fn progress(name: &str, stage: ServerStage) -> Message_ {
                Ok(Operation {
                    name: name.to_owned(),
                    metadata: Some(prost_types::Any {
                        type_url: String::new(),
                        value: reapi::ExecuteOperationMetadata {
                            stage: stage as i32,
                            ..Default::default()
                        }
                        .encode_to_vec(),
                    }),
                    ..Default::default()
                })
            }

            fn finished(name: &str, response: reapi::ExecuteResponse) -> Message_ {
                Ok(Operation {
                    name: name.to_owned(),
                    done: true,
                    result: Some(OpResult::Response(prost_types::Any {
                        type_url: String::new(),
                        value: response.encode_to_vec(),
                    })),
                    ..Default::default()
                })
            }

            /// A successful answer, from the server's cache or from running the action.
            fn answered(name: &str, cached_result: bool) -> Message_ {
                finished(
                    name,
                    reapi::ExecuteResponse {
                        result: Some(reapi::ActionResult {
                            execution_metadata: Some(reapi::ExecutedActionMetadata::default()),
                            ..Default::default()
                        }),
                        cached_result,
                        ..Default::default()
                    },
                )
            }

            /// An operation the server finished with a transient failure.
            fn failed_transiently(name: &str) -> Message_ {
                finished(
                    name,
                    reapi::ExecuteResponse {
                        status: Some(re_grpc_proto::google::rpc::Status {
                            code: tonic::Code::Unavailable as i32,
                            message: "worker lost".to_owned(),
                            ..Default::default()
                        }),
                        result: Some(reapi::ActionResult::default()),
                        ..Default::default()
                    },
                )
            }

            #[derive(Clone, Copy, Debug)]
            enum Cause {
                /// A's stream closes and resuming it reports `NOT_FOUND`.
                Lost,
                /// The server finishes A with `UNAVAILABLE`.
                Transient,
            }

            const CAUSES: [Cause; 2] = [Cause::Lost, Cause::Transient];

            /// A reports `EXECUTING` and then `cause` makes the provider start the action over;
            /// the fresh `Execute` streams `b`. Returns the opener calls and the `resume`
            /// argument each one must have been made with.
            fn restart(cause: Cause, b: Vec<Message_>) -> (Vec<Call>, Vec<Option<String>>) {
                let executing_a = progress(A, ServerStage::Executing);
                match cause {
                    Cause::Lost => (
                        vec![
                            Ok(vec![executing_a]),
                            Err(tonic::Status::not_found("operation lost")),
                            Ok(b),
                        ],
                        vec![None, Some(A.to_owned()), None],
                    ),
                    Cause::Transient => (
                        vec![Ok(vec![executing_a, failed_transiently(A)]), Ok(b)],
                        vec![None, None],
                    ),
                }
            }

            struct Run {
                /// The `resume` argument of each opener call.
                calls: Vec<Option<String>>,
                events: Vec<buck2_data::BuckEvent>,
                digest: String,
            }

            impl Run {
                /// The remote stages the engine reported, in order.
                fn stages(&self) -> Vec<&'static str> {
                    self.events
                        .iter()
                        .filter_map(|e| match &e.data {
                            Some(buck2_data::buck_event::Data::SpanStart(
                                buck2_data::SpanStartEvent {
                                    data:
                                        Some(buck2_data::span_start_event::Data::ExecutorStage(
                                            buck2_data::ExecutorStageStart {
                                                stage:
                                                    Some(
                                                        buck2_data::executor_stage_start::Stage::Re(
                                                            buck2_data::ReStage {
                                                                stage: Some(stage),
                                                            },
                                                        ),
                                                    ),
                                            },
                                        )),
                                },
                            )) => Some(match stage {
                                buck2_data::re_stage::Stage::Queue(..) => "queue",
                                buck2_data::re_stage::Stage::Execute(..) => "execute",
                                buck2_data::re_stage::Stage::Unknown(..) => "unknown",
                                _ => "other",
                            }),
                            _ => None,
                        })
                        .collect()
                }

                /// The request each execution stage recorded.
                fn execute_submissions(&self) -> Vec<Option<u32>> {
                    self.events
                        .iter()
                        .filter_map(|e| match &e.data {
                            Some(buck2_data::buck_event::Data::SpanStart(
                                buck2_data::SpanStartEvent {
                                    data:
                                        Some(buck2_data::span_start_event::Data::ExecutorStage(
                                            buck2_data::ExecutorStageStart {
                                                stage:
                                                    Some(
                                                        buck2_data::executor_stage_start::Stage::Re(
                                                            buck2_data::ReStage {
                                                                stage:
                                                                    Some(
                                                                        buck2_data::re_stage::Stage::Execute(
                                                                            execute,
                                                                        ),
                                                                    ),
                                                            },
                                                        ),
                                                    ),
                                            },
                                        )),
                                },
                            )) => Some(execute.submission),
                            _ => None,
                        })
                        .collect()
                }

                /// The request the report says was answered.
                fn answered_submission(&self) -> Option<u32> {
                    self.events.iter().find_map(|e| match &e.data {
                        Some(buck2_data::buck_event::Data::SpanEnd(end)) => match &end.data {
                            Some(buck2_data::span_end_event::Data::TestRun(run)) => {
                                match run
                                    .command_report
                                    .as_ref()?
                                    .details
                                    .as_ref()?
                                    .command_kind
                                    .as_ref()?
                                    .command
                                    .as_ref()?
                                {
                                    buck2_data::command_execution_kind::Command::RemoteCommand(
                                        remote,
                                    ) => remote.submission,
                                    _ => None,
                                }
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                }

                fn lines(&self, executors: &[&str]) -> String {
                    executors
                        .iter()
                        .map(|e| format!("test.run\tsuite\t{e}(OSFamily=linux)\t{}\n", self.digest))
                        .collect()
                }
            }

            /// Runs the real provider stream over `script` through the real engine producer in a
            /// test-run span whose report the engine derives from the answer.
            async fn run(script: Vec<Call>) -> Run {
                let script = Arc::new(Mutex::new(VecDeque::from(script)));
                let calls = Arc::new(Mutex::new(Vec::new()));
                let receiver = remote_execution::execution_progress_stream(
                    {
                        let script = script.clone();
                        let calls = calls.clone();
                        move |resume: Option<String>| {
                            let script = script.clone();
                            let calls = calls.clone();
                            async move {
                                calls.lock().unwrap().push(resume);
                                let call = script.lock().unwrap().pop_front().unwrap();
                                anyhow::Ok(futures::stream::iter(call?))
                            }
                        }
                    },
                    0,
                )
                .boxed();

                let action_digest = ActionDigest::new_sha1([7; 20], 42);
                let platform = remote_execution::Platform {
                    properties: vec![reapi::platform::Property {
                        name: "OSFamily".to_owned(),
                        value: "linux".to_owned(),
                    }],
                };
                let (mut source, sink) = buck2_events::create_source_sink_pair();
                let dispatcher = EventDispatcher::new(
                    buck2_wrapper_common::invocation_id::TraceId::new(),
                    buck2_events::daemon_id::DaemonId::new(),
                    sink,
                );
                let suite = buck2_data::TestSuite {
                    suite_name: "suite".to_owned(),
                    ..Default::default()
                };
                with_dispatcher_async(dispatcher, {
                    let action_digest = action_digest.clone();
                    async move {
                        span_async(
                            buck2_data::TestRunStart {
                                suite: Some(suite.clone()),
                            },
                            async move {
                                let mut manager = CommandExecutionManager::new(
                                    Box::new(MutexClaimManager::new()),
                                    EventDispatcher::null(),
                                    NoopLivelinessObserver::create(),
                                    WaitingData::new(),
                                );
                                let outcome = report_execution_progress(
                                    receiver,
                                    &action_digest,
                                    &platform,
                                    None,
                                    "buck2-default".to_owned(),
                                    false,
                                    &mut manager,
                                    None,
                                    &ExecutorGlobalKnobs::default(),
                                )
                                .await
                                .unwrap();
                                let ExecuteResponseOrCancelled::Response(response) = outcome else {
                                    panic!("the scripted server answers");
                                };
                                let kind = response.execution_kind(
                                    RemoteCommandExecutionDetails::new(
                                        action_digest.clone(),
                                        None,
                                        None,
                                        buck2_core::execution_types::executor_config::RemoteExecutorUseCase::new(
                                            "buck2-default".to_owned(),
                                        ),
                                        &platform,
                                        false,
                                    ),
                                );
                                let report = buck2_data::CommandExecution {
                                    details: Some(buck2_data::CommandExecutionDetails {
                                        command_kind: Some(kind.to_proto(false)),
                                        ..Default::default()
                                    }),
                                    status: Some(success()),
                                    ..Default::default()
                                };
                                (
                                    (),
                                    buck2_data::TestRunEnd {
                                        suite: Some(suite),
                                        command_report: Some(report),
                                        command_host_sharing_requirements: None,
                                    },
                                )
                            },
                        )
                        .await
                    }
                })
                .await;

                let mut events = Vec::new();
                while let Some(event) = source.try_receive() {
                    if let Some(event) = event.unpack_buck() {
                        events.push(event.event().clone());
                    }
                }
                let calls = calls.lock().unwrap().clone();
                Run {
                    calls,
                    events,
                    digest: action_digest.to_string(),
                }
            }

            /// S1-EVENT-03: A reports `EXECUTING`; A is lost or finished with a transient failure,
            /// so the provider starts a fresh `Execute`, whose operation B is `QUEUED` and then
            /// answered from the server's cache.
            #[tokio::test(start_paused = true)]
            async fn provider_restart_after_execution_reaches_what_ran() {
                for cause in CAUSES {
                    let (script, calls) = restart(
                        cause,
                        vec![progress(B, ServerStage::Queued), answered(B, true)],
                    );
                    let run = run(script).await;
                    assert_eq!(run.calls, calls, "{cause:?}");
                    assert_eq!(run.stages(), vec!["queue", "execute", "queue"], "{cause:?}");
                    assert_eq!(run.execute_submissions(), vec![Some(1)], "{cause:?}");
                    assert_eq!(run.answered_submission(), Some(2), "{cause:?}");
                    assert_eq!(
                        render(&run.events, &options(), false),
                        run.lines(&["re", "re_served_from_cache"]),
                        "{cause:?}"
                    );
                }
            }

            /// S1-EVENT-04: after the restart, B's answer arrives directly, or after B reports a
            /// stage the RE client cannot name (a cache check), `QUEUED`, or `EXECUTING`. In
            /// every form A's execution keeps its plain record, and B's answer is recorded with
            /// its own origin: neither the digest nor the absence of a stage between them shows
            /// that A produced B's answer.
            #[tokio::test(start_paused = true)]
            async fn provider_restart_keeps_the_earlier_execution_however_the_answer_arrives() {
                let forms: Vec<(&str, Vec<Message_>, Vec<&str>, Vec<&str>)> = vec![
                    (
                        "direct cached",
                        vec![answered(B, true)],
                        vec!["queue", "execute"],
                        vec!["re", "re_served_from_cache"],
                    ),
                    (
                        "cache check, then cached",
                        vec![progress(B, ServerStage::CacheCheck), answered(B, true)],
                        vec!["queue", "execute", "unknown"],
                        vec!["re", "re_served_from_cache"],
                    ),
                    (
                        "queued, then cached",
                        vec![progress(B, ServerStage::Queued), answered(B, true)],
                        vec!["queue", "execute", "queue"],
                        vec!["re", "re_served_from_cache"],
                    ),
                    (
                        "executing, then executed",
                        vec![progress(B, ServerStage::Executing), answered(B, false)],
                        vec!["queue", "execute", "execute"],
                        vec!["re", "re"],
                    ),
                ];
                for cause in CAUSES {
                    for (form, b, stages, executors) in forms.clone() {
                        let context = format!("{cause:?} {form}");
                        let (script, calls) = restart(cause, b);
                        let run = run(script).await;
                        assert_eq!(run.calls, calls, "{context}");
                        assert_eq!(run.stages(), stages, "{context}");
                        // A's execution was the first request; the fresh `Execute` the second.
                        let executions = run.stages().iter().filter(|s| **s == "execute").count();
                        assert_eq!(
                            run.execute_submissions(),
                            [Some(1), Some(2)][..executions].to_vec(),
                            "{context}"
                        );
                        assert_eq!(run.answered_submission(), Some(2), "{context}");
                        assert_eq!(
                            render(&run.events, &options(), false),
                            run.lines(&executors),
                            "{context}"
                        );
                        // Filtering still removes every remote record.
                        let skip_remote = WhatRanCommandOptions {
                            options: WhatRanOptions {
                                skip_remote_executions: true,
                                ..Default::default()
                            },
                            failed: false,
                            incomplete: false,
                        };
                        assert_eq!(render(&run.events, &skip_remote, false), "", "{context}");
                    }
                }
            }

            /// Controls: one operation keeps one record with the service's origin, including when
            /// its stream closes and the provider resumes the same operation with
            /// `WaitExecution` (not a new submission).
            #[tokio::test(start_paused = true)]
            async fn one_operation_keeps_one_record_through_the_real_producer() {
                let cases: Vec<(&str, Vec<Call>, Vec<Option<String>>, Vec<&str>, &str)> = vec![
                    (
                        "executing, then cached",
                        vec![Ok(vec![
                            progress(A, ServerStage::Queued),
                            progress(A, ServerStage::Executing),
                            answered(A, true),
                        ])],
                        vec![None],
                        vec!["queue", "execute"],
                        "re_served_from_cache",
                    ),
                    (
                        "executing, then executed",
                        vec![Ok(vec![
                            progress(A, ServerStage::Executing),
                            answered(A, false),
                        ])],
                        vec![None],
                        vec!["queue", "execute"],
                        "re",
                    ),
                    (
                        "executing, resumed, then cached",
                        vec![
                            Ok(vec![progress(A, ServerStage::Executing)]),
                            Ok(vec![answered(A, true)]),
                        ],
                        vec![None, Some(A.to_owned())],
                        vec!["queue", "execute"],
                        "re_served_from_cache",
                    ),
                    (
                        "answered directly from cache",
                        vec![Ok(vec![answered(A, true)])],
                        vec![None],
                        vec!["queue"],
                        "re_served_from_cache",
                    ),
                ];
                for (case, script, calls, stages, executor) in cases {
                    let run = run(script).await;
                    assert_eq!(run.calls, calls, "{case}");
                    assert_eq!(run.stages(), stages, "{case}");
                    // Resuming with `WaitExecution` is not another request.
                    assert!(
                        run.execute_submissions().iter().all(|s| *s == Some(1)),
                        "{case}"
                    );
                    assert_eq!(run.answered_submission(), Some(1), "{case}");
                    assert_eq!(
                        render(&run.events, &options(), false),
                        run.lines(&[executor]),
                        "{case}"
                    );
                }
            }
        }
    }
}
