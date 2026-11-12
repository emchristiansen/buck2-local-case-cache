/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// https://github.com/rust-lang/rust-clippy/issues/12806
#![allow(clippy::unnecessary_to_owned)]

//! Implementation of the `TestOrchestrator` from `buck2_test_api`.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt::Display;
use std::hash::Hash;
use std::hash::Hasher;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_build_api::actions::execute::dice_data::CommandExecutorResponse;
use buck2_build_api::actions::execute::dice_data::DiceHasCommandExecutor;
use buck2_build_api::actions::execute::dice_data::GetReClient;
use buck2_build_api::actions::execute::dice_data::LocalExecution;
use buck2_build_api::actions::impls::run_action_knobs::HasRunActionKnobs;
use buck2_build_api::analysis::calculation::RuleAnalysisCalculation;
use buck2_build_api::artifact_groups::ArtifactGroup;
use buck2_build_api::artifact_groups::ArtifactGroupValues;
use buck2_build_api::artifact_groups::calculation::ArtifactGroupCalculation;
use buck2_build_api::build_signals::HasBuildSignals;
use buck2_build_api::context::HasBuildContextData;
use buck2_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapperImpl;
use buck2_build_api::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use buck2_build_api::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use buck2_build_api::interpreter::rule_defs::cmd_args::CommandLineBuilder;
use buck2_build_api::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use buck2_build_api::interpreter::rule_defs::cmd_args::SingletonCommandLineSink;
use buck2_build_api::interpreter::rule_defs::command_executor_config::ExecutorConfigOrigin;
use buck2_build_api::interpreter::rule_defs::command_executor_config::StarlarkCommandExecutorConfig;
use buck2_build_api::interpreter::rule_defs::provider::builtin::external_runner_test_info::ExternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::external_runner_test_info::OwnedExternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::external_runner_test_info::TestCommandMember;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::CaseResultReuse;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::InternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::OwnedInternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::local_resource_info::OwnedLocalResourceInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::worker_info::WorkerInfo;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use buck2_build_api::interpreter::rule_defs::provider::test_provider::TestProviderKind;
use buck2_build_api::interpreter::rule_defs::required_test_local_resource::StarlarkRequiredTestLocalResource;
use buck2_build_api::keep_going::KeepGoing;
use buck2_build_api::materialize::invocation_re_use_case;
use buck2_build_signals::env::NodeDuration;
use buck2_build_signals::env::WaitingCategory;
use buck2_build_signals::env::WaitingData;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::events::HasEvents;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::liveliness_observer::LivelinessObserver;
use buck2_common::local_resource_state::LocalResourceState;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::execution_types::execution_group::ExecutionGroup;
use buck2_core::execution_types::executor_config::CommandExecutorConfig;
use buck2_core::execution_types::executor_config::CommandGenerationOptions;
use buck2_core::execution_types::executor_config::Executor;
use buck2_core::execution_types::executor_config::LocalExecutorOptions;
use buck2_core::execution_types::executor_config::MetaInternalExtraParams;
use buck2_core::execution_types::executor_config::PathSeparatorKind;
use buck2_core::execution_types::executor_config::RemoteExecutorCustomImage;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::fs::buck_out_path::BuckOutTestPath;
use buck2_core::fs::buck_out_path::BuildArtifactPath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_data::EndOfTestResults;
use buck2_data::SetupLocalResourcesEnd;
use buck2_data::SetupLocalResourcesStart;
use buck2_data::TestDiscovery;
use buck2_data::TestDiscoveryEnd;
use buck2_data::TestDiscoveryStart;
use buck2_data::TestRunEnd;
use buck2_data::TestRunStart;
use buck2_data::TestSessionInfo;
use buck2_data::TestSuite;
use buck2_data::ToProtoMessage;
use buck2_error::BuckErrorContext;
use buck2_error::BuckErrorOptionContext;
use buck2_error::ErrorTag;
use buck2_error::conversion::from_any_with_tag;
use buck2_error::internal_error;
use buck2_events::dispatch::EventDispatcher;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::execute::blocking::HasBlockingExecutor;
use buck2_execute::execute::cache_uploader::CacheUploadInfo;
use buck2_execute::execute::cache_uploader::NoOpCacheUploader;
use buck2_execute::execute::claim::MutexClaimManager;
use buck2_execute::execute::command_executor::CommandExecutor;
use buck2_execute::execute::environment_inheritance::EnvironmentInheritance;
use buck2_execute::execute::executor_stage_async;
use buck2_execute::execute::kind::CommandExecutionKind;
use buck2_execute::execute::manager::CommandExecutionManager;
use buck2_execute::execute::manager::CommandExecutionManagerExt;
use buck2_execute::execute::output::CommandStdStreams;
use buck2_execute::execute::prepared::NoOpCommandOptionalExecutor;
use buck2_execute::execute::prepared::PreparedCommand;
use buck2_execute::execute::request::ActionMetadataBlob;
use buck2_execute::execute::request::CommandExecutionInput;
use buck2_execute::execute::request::CommandExecutionOutput;
use buck2_execute::execute::request::CommandExecutionPaths;
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_execute::execute::request::OutputCreationBehavior;
use buck2_execute::execute::request::WorkerId;
use buck2_execute::execute::request::WorkerSpec;
use buck2_execute::execute::result::CommandCancellationReason;
use buck2_execute::execute::result::CommandExecutionMetadata;
use buck2_execute::execute::result::CommandExecutionReport;
use buck2_execute::execute::result::CommandExecutionResult;
use buck2_execute::execute::result::CommandExecutionStatus;
use buck2_execute::execute::target::CommandExecutionTarget;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_execute::materialize::materializer::MaterializationPurpose;
use buck2_execute::materialize::materializer::MaterializeRequest;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::materialize::materializer::ReadLease;
use buck2_execute::materialize::materializer::WriteRequest;
use buck2_execute::materialize::produce::write_locally;
use buck2_execute_impl::executors::local::EnvironmentBuilder;
use buck2_execute_impl::executors::local::apply_local_execution_environment;
use buck2_execute_impl::executors::local::create_output_dirs;
use buck2_execute_impl::executors::local::materialize_inputs;
use buck2_execute_impl::executors::local::output_paths;
use buck2_execute_impl::executors::local::prep_scratch_path;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_hash::BuckFixedSortedMap;
use buck2_hash::BuckMutInsertionMap;
use buck2_hash::BuckMutInsertionSet;
use buck2_hash::BuckMutMap;
use buck2_hash::BuckMutSet;
use buck2_hash::buck_mut_insertion_set;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use buck2_resource_control::HasResourceControl;
use buck2_test_api::data::ArgValue;
use buck2_test_api::data::ArgValueContent;
use buck2_test_api::data::CasDigest;
use buck2_test_api::data::ConfiguredTargetHandle;
use buck2_test_api::data::DeclaredOutput;
use buck2_test_api::data::ExecuteResponse;
use buck2_test_api::data::ExecutionDetails;
use buck2_test_api::data::ExecutionResult2;
use buck2_test_api::data::ExecutionStatus;
use buck2_test_api::data::ExecutionStream;
use buck2_test_api::data::ExecutorConfigOverride;
use buck2_test_api::data::ExternalRunnerSpecValue;
use buck2_test_api::data::LocalExecutionCommand;
use buck2_test_api::data::Output;
use buck2_test_api::data::PrepareForLocalExecutionResult;
use buck2_test_api::data::RequiredLocalResources;
use buck2_test_api::data::TestResult;
use buck2_test_api::data::TestStage;
use buck2_test_api::data::convert::host_sharing_requirements_to_grpc;
use buck2_test_api::protocol::TestOrchestrator;
use derive_more::From;
use dice::DiceComputations;
use dice::DiceTransaction;
use dice::EqualityBehavior;
use dice::Key;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dice_futures::spawn::spawn_dropcancel;
use display_container::fmt_container;
use display_container::fmt_keyed_container;
use dupe::Dupe;
use dupe::ResultDupedErrExt;
use futures::FutureExt;
use futures::channel::mpsc::UnboundedSender;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use host_sharing::HostSharingRequirements;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::command::InternalRunnerConfig;
use crate::internal_runner::InternalTestOrchestrator;
use crate::local_case_fresh_dirs::FreshDirs;
use crate::local_case_fresh_dirs::ScratchRoot;
use crate::local_resource_api::LocalResourcesSetupResult;
use crate::local_resource_registry::HasLocalResourceRegistry;
use crate::local_resource_setup::TestStageSimple;
use crate::local_resource_setup::required_providers;
use crate::local_result_cache::ConstrainedCase;
use crate::local_result_cache::LocalCaseAdmission;
use crate::local_result_cache::LocalCaseGap;
use crate::local_result_cache::LocalCaseOutcome;
use crate::local_result_cache::LocalCaseRequest;
use crate::local_result_cache::LocalCaseResult;
use crate::local_result_cache::LocalCaseStart;
use crate::local_result_cache::PreparedFacts;
use crate::local_result_cache::RequestFacts;
use crate::local_result_cache::settle_execution;
use crate::remote_storage;
use crate::session::TestSession;
use crate::session::TestSessionOptions;
use crate::translations;

const MAX_SUFFIX_LEN: usize = 1024;

/// The one test provider `buck2 test` runs for a target, chosen by [`OwnedTestInfo::select`].
/// `Internal` runs in-process; `External` is dispatched to the test executor.
pub(crate) enum OwnedTestInfo {
    External(OwnedExternalRunnerTestInfo),
    Internal(OwnedInternalRunnerTestInfo),
}

impl OwnedTestInfo {
    /// The single runner choice for a target under `[test] use_internal_runner`: its
    /// `InternalRunnerTestInfo` when the config selects that provider's framework, otherwise its
    /// `ExternalRunnerTestInfo`. The test driver label-filters, dispatches and builds the
    /// executor spec from this provider, and the orchestrator resolves that spec's command and
    /// environment handles from the same provider, so a target with both providers can never
    /// have its spec produced from one and expanded against the other.
    ///
    /// `None` means the target is not a test. A target whose only provider is an
    /// `InternalRunnerTestInfo` the config does not select is refused: there is no provider to
    /// run it with.
    pub(crate) fn select(
        providers: &FrozenProviderCollectionValue,
        internal_runner_config: &InternalRunnerConfig,
    ) -> buck2_error::Result<Option<Self>> {
        let internal: Option<OwnedInternalRunnerTestInfo> =
            providers.builtin_provider_value::<InternalRunnerTestInfo>()?;
        let external: Option<OwnedExternalRunnerTestInfo> =
            providers.builtin_provider_value::<ExternalRunnerTestInfo>()?;
        match (internal, external) {
            (Some(internal), external) => {
                let test_type = internal.as_ref().value().as_ref().test_type().to_owned();
                if internal_runner_config.should_use(&test_type) {
                    Ok(Some(Self::Internal(internal)))
                } else if let Some(external) = external {
                    Ok(Some(Self::External(external)))
                } else {
                    Err(buck2_error::buck2_error!(
                        ErrorTag::Input,
                        "`[test] use_internal_runner` does not select test type `{}`, and the \
                        target has no `ExternalRunnerTestInfo` to run instead",
                        test_type
                    ))
                }
            }
            (None, Some(external)) => Ok(Some(Self::External(external))),
            (None, None) => Ok(None),
        }
    }

    /// The names whose variables each test case process receives naming a fresh directory.
    fn fresh_dir_env(&self) -> Vec<&str> {
        match self {
            Self::External(_) => Vec::new(),
            Self::Internal(info) => info.as_ref().value().as_ref().fresh_dir_env().collect(),
        }
    }

    /// Which provider this is.
    pub(crate) fn kind(&self) -> TestProviderKind {
        match self {
            Self::External(_) => TestProviderKind::External,
            Self::Internal(_) => TestProviderKind::Internal,
        }
    }

    /// The selected provider's labels, for label filtering.
    pub(crate) fn labels(&self) -> Vec<&str> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().labels().collect(),
            Self::Internal(info) => info.as_ref().value().as_ref().labels().collect(),
        }
    }

    /// Whether the provider requests the strict host-local case store.
    fn requests_local_case_result_caching(&self) -> bool {
        match self {
            Self::External(_) => false,
            Self::Internal(info) => {
                info.as_ref().value().as_ref().case_result_reuse() == CaseResultReuse::HostStore
            }
        }
    }

    /// Whether the provider's test commands read the remote action cache. An Internal provider
    /// does only for remote case result reuse; its runner then confirms each hit with the current
    /// parser before reporting it, and executes a case whose hit it does not confirm again
    /// without reading any cache.
    fn supports_test_execution_caching(&self) -> bool {
        match self {
            Self::External(info) => info
                .as_ref()
                .value()
                .as_ref()
                .supports_test_execution_caching(),
            Self::Internal(info) => {
                info.as_ref().value().as_ref().case_result_reuse()
                    == CaseResultReuse::RemoteActionCache
            }
        }
    }

    fn cli_args_for_stage<'v>(&'v self, stage: &TestStage) -> Vec<&'v dyn CommandLineArgLike<'v>> {
        let filter = |c: TestCommandMember<'v>| -> Option<&'v dyn CommandLineArgLike<'v>> {
            match c {
                TestCommandMember::Literal(..) => None,
                TestCommandMember::Arglike(a) => Some(a),
            }
        };
        match (self, stage) {
            (Self::Internal(info), TestStage::Listing { .. }) => info
                .as_ref()
                .value()
                .as_ref()
                .listing_command()
                .filter_map(filter)
                .collect(),
            (Self::External(info), _) => info
                .as_ref()
                .value()
                .as_ref()
                .command()
                .filter_map(filter)
                .collect(),
            (Self::Internal(info), _) => info
                .as_ref()
                .value()
                .as_ref()
                .command()
                .filter_map(filter)
                .collect(),
        }
    }

    fn env_args<'v>(&'v self) -> BuckMutMap<&'v str, &'v dyn CommandLineArgLike<'v>> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().env().collect(),
            Self::Internal(info) => info.as_ref().value().as_ref().env().collect(),
        }
    }

    fn local_resources(&self) -> BuckMutInsertionMap<&str, Option<&ConfiguredProvidersLabel>> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().local_resources(),
            Self::Internal(info) => info.as_ref().value().as_ref().local_resources(),
        }
    }

    fn required_local_resource_names_for_stage(&self, stage: &TestStageSimple) -> Vec<&str> {
        let filter = |r: &StarlarkRequiredTestLocalResource| match stage {
            TestStageSimple::Listing => r.listing,
            TestStageSimple::Testing => r.execution,
        };
        match self {
            Self::External(info) => info
                .as_ref()
                .value()
                .as_ref()
                .required_local_resources()
                .filter(|r| filter(r))
                .map(|r| r.name.as_str())
                .collect(),
            Self::Internal(info) => info
                .as_ref()
                .value()
                .as_ref()
                .required_local_resources()
                .filter(|r| filter(r))
                .map(|r| r.name.as_str())
                .collect(),
        }
    }

    fn execution_required_local_resource_names(&self) -> Vec<&str> {
        self.required_local_resource_names_for_stage(&TestStageSimple::Testing)
    }

    fn executor_override(&self, key: &str) -> Option<&StarlarkCommandExecutorConfig> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().executor_override(key),
            Self::Internal(info) => info.as_ref().value().as_ref().executor_override(key),
        }
    }

    fn has_executor_overrides(&self) -> bool {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().has_executor_overrides(),
            Self::Internal(info) => info.as_ref().value().as_ref().has_executor_overrides(),
        }
    }

    fn default_executor(&self) -> Option<&StarlarkCommandExecutorConfig> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().default_executor(),
            Self::Internal(info) => info.as_ref().value().as_ref().default_executor(),
        }
    }

    fn execution_group(&self) -> buck2_error::Result<Option<ExecutionGroup>> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().execution_group(),
            Self::Internal(info) => info.as_ref().value().as_ref().execution_group(),
        }
    }

    fn run_from_project_root(&self) -> bool {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().run_from_project_root(),
            Self::Internal(info) => info.as_ref().value().as_ref().run_from_project_root(),
        }
    }

    fn use_project_relative_paths(&self) -> bool {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().use_project_relative_paths(),
            Self::Internal(info) => info.as_ref().value().as_ref().use_project_relative_paths(),
        }
    }

    fn worker(&self) -> Option<&WorkerInfo<'_>> {
        match self {
            Self::External(info) => info.as_ref().value().as_ref().worker(),
            Self::Internal(info) => info.as_ref().value().as_ref().worker(),
        }
    }

    fn has_static_listing_label(&self) -> bool {
        match self {
            Self::External(info) => info
                .as_ref()
                .value()
                .as_ref()
                .labels()
                .any(|l| l == "static-listing"),
            Self::Internal(info) => info
                .as_ref()
                .value()
                .as_ref()
                .labels()
                .any(|l| l == "static-listing"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ExecutorMessage {
    TestResult(TestResult),
    ExitCode(i32),
    InfoMessage(String),
}

pub struct BuckTestOrchestrator<'a: 'static> {
    dice: DiceTransaction,
    session: Arc<TestSession>,
    results_channel: UnboundedSender<buck2_error::Result<ExecutorMessage>>,
    events: EventDispatcher,
    liveliness_observer: Arc<dyn LivelinessObserver>,
    cancellations: &'a CancellationContext,
    re_client: Arc<remote_storage::ReClientWithCache>,
    internal_runner_config: InternalRunnerConfig,
}

impl<'a> BuckTestOrchestrator<'a> {
    pub(crate) async fn new(
        dice: DiceTransaction,
        session: Arc<TestSession>,
        liveliness_observer: Arc<dyn LivelinessObserver>,
        results_channel: UnboundedSender<buck2_error::Result<ExecutorMessage>>,
        cancellations: &'a CancellationContext,
        internal_runner_config: InternalRunnerConfig,
    ) -> buck2_error::Result<BuckTestOrchestrator<'a>> {
        let events = dice.per_transaction_data().get_dispatcher().dupe();
        let re_client = Arc::new(remote_storage::ReClientWithCache::new(
            dice.per_transaction_data().get_re_client().dupe(),
        ));
        Ok(Self::from_parts(
            dice,
            session,
            liveliness_observer,
            results_channel,
            events,
            cancellations,
            re_client,
            internal_runner_config,
        ))
    }

    fn from_parts(
        dice: DiceTransaction,
        session: Arc<TestSession>,
        liveliness_observer: Arc<dyn LivelinessObserver>,
        results_channel: UnboundedSender<buck2_error::Result<ExecutorMessage>>,
        events: EventDispatcher,
        cancellations: &'a CancellationContext,
        re_client: Arc<remote_storage::ReClientWithCache>,
        internal_runner_config: InternalRunnerConfig,
    ) -> BuckTestOrchestrator<'a> {
        Self {
            dice,
            session,
            results_channel,
            events,
            liveliness_observer,
            cancellations,
            re_client,
            internal_runner_config,
        }
    }

    async fn require_alive(
        liveliness_observer: Arc<dyn LivelinessObserver>,
    ) -> Result<(), Cancelled> {
        if !liveliness_observer.is_alive().await {
            return Err(Cancelled {
                ..Default::default()
            });
        }

        Ok(())
    }

    /// Whether to exempt this action from network isolation when it runs locally.
    ///
    /// Static listing's enumeration tool (gtest-list-tests, coral, ...) is a DotSlash
    /// stub that needs the network to resolve. On RE it can still resolve, because RE
    /// has a DotSlash that works under the allowed isolation modes, so the listing
    /// action inherits the normal executor policy and needs no override. Locally the
    /// forkserver would put a restricted policy in a network namespace where the stub
    /// can't resolve, so the local listing action must run without isolation. Has no
    /// effect on remote execution.
    fn disable_local_network_isolation(stage: &TestStage, test_info: &OwnedTestInfo) -> bool {
        matches!(stage, TestStage::Listing { .. }) && test_info.has_static_listing_label()
    }

    async fn execute2(
        &self,
        stage: TestStage,
        test_target: ConfiguredTargetHandle,
        cmd: Vec<ArgValue>,
        env: BuckFixedSortedMap<String, ArgValue>,
        timeout: Duration,
        host_sharing_requirements: HostSharingRequirements,
        pre_create_dirs: Vec<DeclaredOutput>,
        executor_override: Option<ExecutorConfigOverride>,
        required_local_resources: RequiredLocalResources,
        disable_test_execution_caching: bool,
        local_case_request: Option<LocalCaseRequest>,
    ) -> Result<(ExecutionResult2, LocalCaseOutcome), ExecuteError> {
        Self::require_alive(self.liveliness_observer.dupe()).await?;

        let test_target = self.session.get(test_target)?;

        // The runner reads a run's outputs out of process, so their lease lives in the session
        // until the runner comes back to this root; the rerun then takes the root for writing,
        // which the lease it replaces would block.
        let output_root = ProjectRelativePathBuf::from(
            resolve_output_root(&mut self.dice.dupe().ctx(), &test_target, &stage).await?,
        );
        self.session.release(&output_root);

        let fs = self.dice.ctx().get_artifact_fs().await?;
        let pre_create_dirs = Arc::new(pre_create_dirs);

        let ExecuteData {
            stdout,
            stderr,
            status,
            timing,
            execution_kind,
            outputs,
            command_execution,
            local_case,
            ..
        } = {
            let key = TestExecutionKey {
                test_target,
                cmd: Arc::new(cmd),
                env: Arc::new(env),
                executor_override: executor_override.map(Arc::new),
                required_local_resources: Arc::new(required_local_resources),
                pre_create_dirs: pre_create_dirs.dupe(),
                stage: Arc::new(stage),
                options: self.session.options(),
                timeout,
                host_sharing_requirements: host_sharing_requirements.into(),
                disable_test_execution_caching,
            };
            // A fresh-directory execution holds its scratch path's lease until its child has
            // terminated, so it too needs a task that outlives a dropped caller.
            let fresh_dirs_declared = matches!(key.stage.as_ref(), TestStage::Testing { .. })
                && !Self::get_test_info(
                    &mut self.dice.dupe().ctx(),
                    &key.test_target,
                    &self.internal_runner_config,
                )
                .await?
                .fresh_dir_env()
                .is_empty();
            let own_task = matches!(local_case_request, Some(LocalCaseRequest::Track { .. }))
                || fresh_dirs_declared;
            match local_case_request {
                local_case_request if own_task => {
                    // A tracked case holds its store generation from before its child starts
                    // until settlement, and a fresh-directory execution its scratch path's lease
                    // until its child has terminated. Cancelling the test command drops this
                    // future, so the case runs as its own task with a real cancellation context:
                    // on cancellation the task stays alive inside the critical section around
                    // execution, the executor kills and reaps the child and returns its
                    // collected streams, and only then is the generation settled with them and
                    // the scratch path released.
                    let dice = self.dice.dupe();
                    let liveliness_observer = self.liveliness_observer.dupe();
                    let internal_runner_config = self.internal_runner_config.clone();
                    let data = self.dice.per_transaction_data();
                    spawn_dropcancel(
                        move |cancellations| {
                            async move {
                                prepare_and_execute(
                                    &mut dice.dupe().ctx(),
                                    cancellations,
                                    key,
                                    liveliness_observer,
                                    &internal_runner_config,
                                    local_case_request,
                                )
                                .await
                            }
                            .boxed()
                        },
                        &*data.spawner,
                        data,
                    )
                    .await
                }
                local_case_request => {
                    prepare_and_execute(
                        &mut self.dice.dupe().ctx(),
                        self.cancellations,
                        key,
                        self.liveliness_observer.dupe(),
                        &self.internal_runner_config,
                        local_case_request,
                    )
                    .await
                }
            }
        }?;

        Self::require_alive(self.liveliness_observer.dupe()).await?;

        let mut output_map = BuckMutMap::default();
        let mut outputs_to_materialize = vec![];

        let remote_storage_config_update_futures = FuturesUnordered::new();

        for (test_path, artifact) in outputs {
            let project_relative_path = fs.buck_out_path_resolver().resolve_test(&test_path);
            let output_name = test_path.into_path().into();
            // It's OK to search iteratively here because there will be few entries in `pre_create_dirs`
            let remote_storage_config = pre_create_dirs
                .iter()
                .find(|&x| x.name == output_name)
                .map_or_else(Default::default, |x| x.remote_storage_config.dupe());
            match (
                remote_storage_config.supports_remote,
                execution_kind.as_ref(),
                translations::convert_artifact(output_name.clone().into_string(), &artifact),
            ) {
                // This condition checks that a downstream consumer supports
                // remote outputs AND the output is actually in CAS.
                //
                // TODO(arr): is there a better way to check that the output is
                // in CAS other than checking that the command was executed on
                // RE? Alternatively, when we make buck upload local testing
                // artifacts to CAS, we can remove this condition altogether.
                (true, Some(CommandExecutionKind::Remote { .. }), Some(remote_object)) => {
                    let re_client = self.re_client.clone();
                    let future = async move {
                        let _unused = re_client
                            .apply_config(&artifact, &remote_storage_config)
                            .await;
                        (output_name, remote_object)
                    };
                    remote_storage_config_update_futures.push(future);
                }
                _ => {
                    outputs_to_materialize.push((project_relative_path.clone(), artifact.dupe()));
                    let abs_path = fs.fs().resolve(&project_relative_path);
                    output_map.insert(output_name, Output::LocalPath(abs_path));
                }
            };
        }
        let results: Vec<_> = remote_storage_config_update_futures.collect().await;
        for result in results {
            output_map.insert(result.0, Output::RemoteObject(result.1));
        }

        // Request materialization in case this ran on RE. Eventually Tpx should be able to
        // understand remote outputs but currently we don't have this.
        let re_use_case = invocation_re_use_case(&self.dice.ctx());
        let response = self
            .dice
            .per_transaction_data()
            .get_materializer()
            .materialize(MaterializeRequest {
                artifacts: outputs_to_materialize,
                outputs: Vec::new(),
                purpose: MaterializationPurpose::IntermediateOnly,
                re_use_case,
            })
            .await
            .buck_error_context("Error materializing test outputs")?;
        for result in response.results {
            result.buck_error_context("Error materializing test outputs")?;
        }
        self.session.hold(output_root, response.lease);

        Ok((
            ExecutionResult2 {
                status,
                stdout,
                stderr,
                outputs: output_map,
                start_time: timing.start_time,
                execution_time: timing.execution_time,
                execution_details: ExecutionDetails {
                    execution_kind: execution_kind.map(|k| k.to_proto(false)),
                },
                max_memory_used_bytes: timing.execution_stats.and_then(|s| s.memory_peak),
                command_execution,
            },
            local_case,
        ))
    }

    /// Whether a cacheable listing may reuse its DICE result. An admitted local-workspace test
    /// may read files of its live workspace that are no input of its listing key, so its listing
    /// runs again for every request; every other cacheable listing reuses a successful result.
    /// This resolves the same provider, effective test execution caching and executor admission
    /// that executing the listing uses, before any DICE lookup.
    async fn listing_reuses_dice_result(
        dice: &mut DiceComputations<'_>,
        key: &TestExecutionKey,
        internal_runner_config: &InternalRunnerConfig,
    ) -> buck2_error::Result<bool> {
        let test_info = Self::get_test_info(dice, &key.test_target, internal_runner_config).await?;
        let effective_test_execution_caching =
            test_info.supports_test_execution_caching() && !key.disable_test_execution_caching;
        let (_, local_execution) = Self::provider_executor_admission(
            dice,
            &test_info,
            key.executor_override.as_deref(),
            key.stage.as_ref(),
            effective_test_execution_caching,
        )
        .await?;
        Ok(local_execution != LocalExecution::AdmittedLocalWorkspaceTest)
    }

    async fn prepare_and_execute_no_dice(
        dice: &mut DiceComputations<'_>,
        key: TestExecutionKey,
        liveliness_observer: Arc<dyn LivelinessObserver>,
        cancellation: &CancellationContext,
        internal_runner_config: &InternalRunnerConfig,
        local_case_request: Option<LocalCaseRequest>,
    ) -> Result<ExecuteData, ExecuteError> {
        let TestExecutionKey {
            test_target,
            cmd,
            env,
            executor_override,
            required_local_resources,
            pre_create_dirs,
            stage,
            options,
            timeout,
            host_sharing_requirements,
            disable_test_execution_caching,
        } = key;
        // The test executor decides when to ask buck2 to run a test, so the time from the test's
        // dependencies finishing until now is the executor's scheduling.
        let mut waiting_data =
            WaitingData::with_initial_category(WaitingCategory::TestExecutorScheduling);
        waiting_data.start_waiting_category_now(WaitingCategory::PreparingAction);
        let fs = dice.get_artifact_fs().await?;
        let test_info = Self::get_test_info(dice, &test_target, internal_runner_config).await?;
        let local_case_admission = LocalCaseAdmission::for_stage(
            stage.as_ref(),
            &test_target.to_string(),
            test_info.requests_local_case_result_caching(),
            disable_test_execution_caching,
            Self::local_case_result_cache_root(dice).await?.as_deref(),
            local_case_request.as_ref(),
        );
        if let LocalCaseAdmission::Refused(gap) = &local_case_admission {
            // Before executor preparation, resource setup or a child.
            return Err(ExecuteError::Error(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Local case result reuse refused {} ({}) before execution: {}",
                test_target,
                stage,
                gap
            )));
        }
        let effective_test_execution_caching =
            test_info.supports_test_execution_caching() && !disable_test_execution_caching;
        let disable_local_network_isolation =
            Self::disable_local_network_isolation(stage.as_ref(), &test_info);
        let test_executor = Self::get_test_executor(
            dice,
            &test_target,
            &test_info,
            executor_override,
            fs,
            &stage,
            effective_test_execution_caching,
        )
        .await?;
        let test_executable_expanded = Self::expand_test_executable(
            dice,
            &test_target,
            &test_info,
            Cow::Borrowed(&cmd),
            Cow::Borrowed(&env),
            Cow::Borrowed(&pre_create_dirs),
            &test_executor.executor().executor_fs(),
            &stage,
            options,
        )
        .boxed()
        .await?;
        let ExpandedTestExecutable {
            cwd,
            cmd: expanded_cmd,
            env: expanded_env,
            ensured_inputs,
            supports_re,
            declared_outputs,
            argsfile_inputs,
            worker,
        } = test_executable_expanded;
        // A configured scratch root is validated for every request that would use it; an invalid
        // one refuses the request rather than falling back to `buck-out`.
        let scratch_root = match Self::local_case_scratch_root(dice).await? {
            Some(root)
                if matches!(stage.as_ref(), TestStage::Testing { .. })
                    && !test_info.fresh_dir_env().is_empty() =>
            {
                Some(ScratchRoot::validate(root).await?)
            }
            _ => None,
        };
        let fresh_dirs = FreshDirs::for_stage(
            &test_target,
            stage.as_ref(),
            test_info.fresh_dir_env(),
            fs,
            scratch_root.as_ref(),
        )?;
        if let Some(fresh_dirs) = &fresh_dirs {
            // The executor sets the variables itself, after every other variable.
            if let Some(name) = fresh_dirs
                .names()
                .find(|name| expanded_env.contains_key(*name))
            {
                return Err(ExecuteError::Error(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "`fresh_dir_env` entry `{}` of {} is also set in the test environment",
                    name,
                    test_target
                )));
            }
            if worker.is_some() {
                return Err(ExecuteError::Error(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "{} declares `fresh_dir_env`, which a test worker cannot provide",
                    test_target
                )));
            }
        }

        let input_deps_action_keys: Vec<_> = ensured_inputs
            .iter()
            .flat_map(|(_, agv)| {
                agv.iter()
                    .filter_map(|(artifact, _)| artifact.action_key().map(|k| k.dupe()))
            })
            .collect::<BuckMutSet<_>>() // dedupe
            .into_iter()
            .collect();

        let mut executor_preference = Self::executor_preference(options, supports_re)?;
        if fresh_dirs.is_some() {
            // Only the local executor creates the fresh directories, and their values are local
            // absolute paths.
            executor_preference = executor_preference.and(ExecutorPreference::LocalRequired)?;
            if !test_executor
                .executor()
                .is_local_execution_possible(executor_preference)
            {
                return Err(ExecuteError::Error(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "{} declares `fresh_dir_env`, which needs local execution, but its executor \
                    cannot run it locally",
                    test_target
                )));
            }
        }
        let required_resources = if test_executor
            .executor()
            .is_local_execution_possible(executor_preference)
        {
            let simple_stage = stage.as_ref().into();

            let available_resources: BuckMutMap<_, _> =
                test_info.local_resources().into_iter().collect();
            let rule_required_names =
                test_info.required_local_resource_names_for_stage(&simple_stage);
            let required_providers = {
                required_providers(
                    dice,
                    available_resources,
                    rule_required_names,
                    &required_local_resources,
                )
                .await?
            };
            if required_providers.is_empty() {
                // No setup command runs, so no local executor is needed. Resource setup is not
                // a test command, and is never admitted to run locally under `--remote-only`.
                vec![]
            } else {
                let setup_local_resources_executor = Self::get_local_executor(dice, fs)?;
                // If some timeout is neeeded, use the same value as for the test itself which is better than nothing.
                Self::setup_local_resources(
                    dice,
                    cancellation,
                    required_providers,
                    setup_local_resources_executor,
                    timeout,
                    liveliness_observer.dupe(),
                )
                .await?
            }
        } else {
            vec![]
        };
        let execution_request = Self::create_command_execution_request(
            dice,
            cwd,
            expanded_cmd,
            expanded_env,
            ensured_inputs,
            declared_outputs,
            argsfile_inputs,
            fs,
            Some(timeout),
            Some(host_sharing_requirements),
            Some(executor_preference),
            required_resources,
            worker,
            test_executor.re_dynamic_image(),
            test_executor.meta_internal_extra_params(),
            disable_local_network_isolation,
            fresh_dirs.as_ref(),
        )
        .boxed()
        .await?;
        let (execution_request, local_case) = match local_case_admission {
            LocalCaseAdmission::NotRequested => (execution_request, None),
            LocalCaseAdmission::Ineligible(gap) => {
                Self::report_local_case_gap(&test_target, &stage, gap);
                (execution_request, None)
            }
            LocalCaseAdmission::Refused(_) => {
                return Err(ExecuteError::Error(internal_error!(
                    "refused local case reached request construction"
                )));
            }
            LocalCaseAdmission::Candidate(candidate) => {
                let (request, constrained) = candidate.constrain(
                    execution_request,
                    RequestFacts {
                        project_fs: fs.fs(),
                        buck_out: fs
                            .buck_out_path_resolver()
                            .root()
                            .iter()
                            .map(|c| c.as_str().to_owned())
                            .collect(),
                    },
                    fresh_dirs.as_ref(),
                    |preference| {
                        test_executor
                            .executor()
                            .is_local_execution_possible(preference)
                    },
                );
                match constrained {
                    Ok(case) => (request, Some(case)),
                    Err(gap) => {
                        Self::report_local_case_gap(&test_target, &stage, gap);
                        (request, None)
                    }
                }
            }
        };
        let result = Self::execute_request(
            dice,
            cancellation,
            &test_target,
            &stage,
            test_executor.executor(),
            execution_request,
            liveliness_observer.dupe(),
            test_executor.re_cache_enabled(),
            effective_test_execution_caching,
            local_case,
            fresh_dirs,
            waiting_data,
        )
        .boxed()
        .await?;

        if let Some(signals) = dice.per_transaction_data().get_build_signals() {
            let duration = NodeDuration {
                user: result.timing.execution_time,
                total: result.timing.time_span,
                queue: result.timing.queue_duration,
            };

            match stage.as_ref() {
                TestStage::Listing { suite, .. } => {
                    signals.test_listing(
                        test_target.target().dupe(),
                        suite.to_owned(),
                        duration.to_owned(),
                        &input_deps_action_keys,
                        result.waiting_data.clone(),
                    );
                }
                TestStage::Testing {
                    suite,
                    testcases,
                    variant,
                    ..
                } => {
                    signals.test_execution(
                        test_target.target().dupe(),
                        suite.to_owned(),
                        testcases,
                        variant.to_owned(),
                        duration,
                        &input_deps_action_keys,
                        result.waiting_data.clone(),
                    );
                }
            }
        }

        Ok(result)
    }
}

#[derive(Clone, Dupe, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct TestExecutionKey {
    test_target: ConfiguredProvidersLabel,
    cmd: Arc<Vec<ArgValue>>,
    env: Arc<BuckFixedSortedMap<String, ArgValue>>,
    executor_override: Option<Arc<ExecutorConfigOverride>>,
    required_local_resources: Arc<RequiredLocalResources>,
    pre_create_dirs: Arc<Vec<DeclaredOutput>>,
    stage: Arc<TestStage>,
    options: TestSessionOptions,
    timeout: Duration,
    host_sharing_requirements: Arc<HostSharingRequirements>,
    disable_test_execution_caching: bool,
}

#[async_trait]
impl Key for TestExecutionKey {
    type Value = Result<ExecuteData, ExecuteError>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        cancellations: &CancellationContext,
    ) -> Self::Value {
        let cell_resolver = ctx.get_cell_resolver().await.map_err(ExecuteError::Error)?;
        let config = InternalRunnerConfig::parse(
            ctx.get_legacy_config_property(
                cell_resolver.root_cell(),
                BuckconfigKeyRef {
                    section: "test",
                    property: "use_internal_runner",
                },
            )
            .await
            .map_err(ExecuteError::Error)?
            .as_deref(),
        );
        cancellations
            .with_structured_cancellation(|observer| {
                async move {
                    BuckTestOrchestrator::prepare_and_execute_no_dice(
                        ctx,
                        self.dupe(),
                        Arc::new(observer),
                        cancellations,
                        &config,
                        None,
                    )
                    .await
                }
                .boxed()
            })
            .await
    }

    fn equality_behavior() -> EqualityBehavior<Self::Value> {
        EqualityBehavior::AlwaysUnequal
    }

    fn validity(x: &Self::Value) -> bool {
        // We don't want to cache any failed listings
        x.as_ref()
            .is_ok_and(|f| f.status == ExecutionStatus::Finished { exitcode: 0 })
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn prepare_and_execute(
    ctx: &mut DiceComputations<'_>,
    cancellation: &CancellationContext,
    key: TestExecutionKey,
    liveliness_observer: Arc<dyn LivelinessObserver>,
    internal_runner_config: &InternalRunnerConfig,
    local_case_request: Option<LocalCaseRequest>,
) -> Result<ExecuteData, ExecuteError> {
    let execute_on_dice = match key.stage.as_ref() {
        TestStage::Listing { cacheable, .. } => {
            *cacheable
                && BuckTestOrchestrator::listing_reuses_dice_result(
                    ctx,
                    &key,
                    internal_runner_config,
                )
                .await?
        }
        TestStage::Testing { .. } => false,
    };
    if execute_on_dice {
        let result = tokio::select! {
            _ = liveliness_observer.while_alive() => {
                Err(ExecuteError::Cancelled(Cancelled{..Default::default()}))
            }
            result = prepare_and_execute_dice(ctx, &key) => {
                result
            }
        }?;
        Ok((*result).clone())
    } else {
        Ok(BuckTestOrchestrator::prepare_and_execute_no_dice(
            ctx,
            key,
            liveliness_observer,
            cancellation,
            internal_runner_config,
            local_case_request,
        )
        .await?)
    }
}

async fn prepare_and_execute_dice<'d>(
    ctx: &mut DiceComputations<'d>,
    key: &TestExecutionKey,
) -> Result<&'d ExecuteData, ExecuteError> {
    ctx.compute(key)
        .await
        .map_err(buck2_error::Error::from)?
        .as_ref()
        .duped_err()
}

impl Display for TestExecutionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "test_target = {}, ", self.test_target)?;
        fmt_container(f, "cmd = [", "], ", self.cmd.as_ref())?;
        fmt_keyed_container(f, "env = {", "}, ", ",", self.env.as_ref())?;
        fmt_container(
            f,
            "executor_override = [",
            "], ",
            self.executor_override.iter(),
        )?;
        write!(
            f,
            "required_local_resources = {}, ",
            self.required_local_resources.as_ref(),
        )?;
        fmt_container(f, "pre_create_dirs = [", "], ", self.pre_create_dirs.iter())?;
        write!(
            f,
            "stage = {}, options = {}, timeout = {}, host_sharing_requirements = {}",
            self.stage,
            self.options,
            self.timeout.as_millis(),
            self.host_sharing_requirements
        )
    }
}

/// The root cell's two closed `[test]` local admissions. Each admits only its own provider kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LocalTestAdmissions {
    /// `[test] local_internal_cases`.
    internal_cases: bool,
    /// `[test] local_workspace_tests`.
    workspace_tests: bool,
}

/// Whether a test command keeps the cache checker or uploader the executor factory provided, or
/// replaces it with a no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestCachePath {
    Provided,
    NoOp,
}

/// The outcome of checking a constrained case's launch before its action is prepared.
enum LaunchCheck {
    Eligible(ConstrainedCase),
    /// The case executes with its legacy request and publishes nothing.
    Ineligible(ConstrainedCase),
    /// The check itself failed: the case's error, with no child.
    Failed(&'static str, buck2_error::Error),
}

/// Timing for a reused result: it waited for no child, so it has no execution interval of its
/// own. The original execution time remains in the entry's manifest.
fn reuse_timing() -> CommandExecutionMetadata {
    CommandExecutionMetadata {
        time_span: buck2_util::time_span::TimeSpan::empty_now(),
        execution_time: Duration::ZERO,
        start_time: std::time::SystemTime::now(),
        execution_stats: None,
        input_materialization_duration: Duration::ZERO,
        hashing_duration: Duration::ZERO,
        hashed_artifacts_count: 0,
        queue_duration: None,
        suspend_duration: None,
        suspend_count: None,
    }
}

struct PreparedLocalResourceSetupContext {
    pub target: ConfiguredTargetLabel,
    pub execution_request: CommandExecutionRequest,
    pub env_var_mapping: BuckMutInsertionMap<String, String>,
}

#[derive(Clone, Dupe, Allocative)]
enum CancellationReason {
    NotSpecified,
    ReQueueTimeout,
}

#[derive(Default, Clone, Dupe, Allocative)]
struct Cancelled {
    reason: Option<CancellationReason>,
}

// NOTE: This doesn't implement Error so that we can't accidentally lose the Cancelled variant.
#[derive(From, Clone, Dupe, Allocative)]
enum ExecuteError {
    Error(buck2_error::Error),
    Cancelled(Cancelled),
}

impl BuckTestOrchestrator<'_> {
    /// Execute one request, returning the local case reuse or captured success for the internal
    /// runner alongside the ordinary response.
    async fn execute_with_local_case(
        &self,
        stage: TestStage,
        test_target: ConfiguredTargetHandle,
        cmd: Vec<ArgValue>,
        env: BuckFixedSortedMap<String, ArgValue>,
        timeout: Duration,
        host_sharing_requirements: HostSharingRequirements,
        pre_create_dirs: Vec<DeclaredOutput>,
        executor_override: Option<ExecutorConfigOverride>,
        required_local_resources: RequiredLocalResources,
        disable_test_execution_caching: bool,
        local_case_request: Option<LocalCaseRequest>,
    ) -> buck2_error::Result<(ExecuteResponse, LocalCaseOutcome)> {
        let res = BuckTestOrchestrator::execute2(
            self,
            stage,
            test_target,
            cmd,
            env,
            timeout,
            host_sharing_requirements,
            pre_create_dirs,
            executor_override,
            required_local_resources,
            disable_test_execution_caching,
            local_case_request,
        )
        .await;

        match res {
            Ok((r, local_case)) => Ok((ExecuteResponse::Result(r), local_case)),
            Err(ExecuteError::Cancelled(cancelled)) => Ok((
                ExecuteResponse::Cancelled(match cancelled.reason {
                    Some(CancellationReason::NotSpecified) => {
                        Some(buck2_test_api::data::CancellationReason::NotSpecified)
                    }
                    Some(CancellationReason::ReQueueTimeout) => {
                        Some(buck2_test_api::data::CancellationReason::ReQueueTimeout)
                    }
                    None => None,
                }),
                LocalCaseOutcome::default(),
            )),
            Err(ExecuteError::Error(e)) => Err(e),
        }
    }
}

#[async_trait]
impl InternalTestOrchestrator for BuckTestOrchestrator<'_> {
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
    ) -> buck2_error::Result<(ExecuteResponse, LocalCaseOutcome)> {
        self.execute_with_local_case(
            stage,
            test_target,
            cmd,
            env,
            timeout,
            HostSharingRequirements::default(),
            Vec::new(),
            None,
            required_local_resources,
            disable_test_execution_caching,
            local_case,
        )
        .await
    }
}

#[async_trait]
impl TestOrchestrator for BuckTestOrchestrator<'_> {
    async fn execute2(
        &self,
        stage: TestStage,
        test_target: ConfiguredTargetHandle,
        cmd: Vec<ArgValue>,
        env: BuckFixedSortedMap<String, ArgValue>,
        timeout: Duration,
        host_sharing_requirements: HostSharingRequirements,
        pre_create_dirs: Vec<DeclaredOutput>,
        executor_override: Option<ExecutorConfigOverride>,
        required_local_resources: RequiredLocalResources,
        disable_test_execution_caching: bool,
    ) -> buck2_error::Result<ExecuteResponse> {
        // Only the internal runner can confirm a captured local case; dropping it here records
        // its generation as unsuccessful.
        let (response, _local_case) = self
            .execute_with_local_case(
                stage,
                test_target,
                cmd,
                env,
                timeout,
                host_sharing_requirements,
                pre_create_dirs,
                executor_override,
                required_local_resources,
                disable_test_execution_caching,
                None,
            )
            .await?;
        Ok(response)
    }

    async fn report_test_result(&self, r: TestResult) -> buck2_error::Result<()> {
        let event = buck2_data::instant_event::Data::TestResult(translations::convert_test_result(
            r.clone(),
            &self.session,
        )?);
        self.events.instant_event(event);
        self.results_channel
            .unbounded_send(Ok(ExecutorMessage::TestResult(r)))
            .map_err(|_| {
                buck2_error::internal_error!("Test result was received after end-of-tests")
            })?;
        Ok(())
    }

    async fn report_tests_discovered(
        &self,
        test_target: ConfiguredTargetHandle,
        suite: String,
        names: Vec<String>,
    ) -> buck2_error::Result<()> {
        let test_target = self.session.get(test_target)?;

        self.events.instant_event(TestDiscovery {
            data: Some(buck2_data::test_discovery::Data::Tests(TestSuite {
                suite_name: suite,
                test_names: names,
                target_label: Some(test_target.target().as_proto()),
            })),
        });

        Ok(())
    }

    async fn report_test_session(
        &self,
        session_info: String,
        test_session_id: Option<String>,
    ) -> buck2_error::Result<()> {
        self.events.instant_event(TestDiscovery {
            data: Some(buck2_data::test_discovery::Data::Session(TestSessionInfo {
                info: session_info,
                test_session_id,
            })),
        });

        Ok(())
    }

    async fn end_of_test_results(&self, exit_code: i32) -> buck2_error::Result<()> {
        self.session.release_all();
        self.events.instant_event(EndOfTestResults { exit_code });
        self.results_channel
            .unbounded_send(Ok(ExecutorMessage::ExitCode(exit_code)))
            .map_err(|_| buck2_error::internal_error!("end_of_tests was received twice"))?;
        self.results_channel.close_channel();
        Ok(())
    }

    async fn prepare_for_local_execution(
        &self,
        stage: TestStage,
        test_target: ConfiguredTargetHandle,
        cmd: Vec<ArgValue>,
        env: BuckFixedSortedMap<String, ArgValue>,
        pre_create_dirs: Vec<DeclaredOutput>,
        required_local_resources: RequiredLocalResources,
    ) -> buck2_error::Result<PrepareForLocalExecutionResult> {
        let test_target = self.session.get(test_target)?;

        // As in `execute2`: the runner reads the inputs out of process, under a lease the
        // session holds until the runner comes back to this root.
        let output_root = ProjectRelativePathBuf::from(
            resolve_output_root(&mut self.dice.dupe().ctx(), &test_target, &stage).await?,
        );
        self.session.release(&output_root);

        let fs = self.dice.ctx().get_artifact_fs().await?;

        let test_info = Self::get_test_info(
            &mut self.dice.dupe().ctx(),
            &test_target,
            &self.internal_runner_config,
        )
        .await?;
        let disable_local_network_isolation =
            Self::disable_local_network_isolation(&stage, &test_info);

        // In contrast from actual test execution we do not check if local execution is possible.
        // We leave that decision to actual local execution runner that requests local execution preparation.
        let setup_local_resources_executor =
            Self::get_local_executor(&mut self.dice.dupe().ctx(), fs)?;
        let available_resources: BuckMutMap<_, _> =
            test_info.local_resources().into_iter().collect();
        let rule_required_names = test_info.execution_required_local_resource_names();
        let providers = {
            required_providers(
                &mut self.dice.dupe().ctx(),
                available_resources,
                rule_required_names,
                &required_local_resources,
            )
            .await?
        };
        let executor_fs = setup_local_resources_executor.executor_fs();
        let setup_commands: Vec<PreparedLocalResourceSetupContext> = self
            .dice
            .dupe()
            .ctx()
            .try_compute_join(providers, async |dice, provider| {
                Self::prepare_local_resource(dice, provider, fs, &executor_fs, Duration::default())
                    .await
            })
            .await?;

        // Tests are not run, so there is no executor override.
        let test_executor = Self::get_test_executor(
            &mut self.dice.dupe().ctx(),
            &test_target,
            &test_info,
            None,
            fs,
            &stage,
            false,
        )
        .await?;
        let test_executable_expanded = Self::expand_test_executable(
            &mut self.dice.dupe().ctx(),
            &test_target,
            &test_info,
            Cow::Owned(cmd),
            Cow::Owned(env),
            Cow::Owned(pre_create_dirs),
            &test_executor.executor().executor_fs(),
            &stage,
            self.session.options(),
        )
        .await?;

        let ExpandedTestExecutable {
            cwd,
            cmd: expanded_cmd,
            env: expanded_env,
            ensured_inputs,
            supports_re: _,
            declared_outputs,
            argsfile_inputs,
            worker,
        } = test_executable_expanded;
        // The command runs out of process after this returns, so no scratch lease could cover it.
        if FreshDirs::for_stage(&test_target, &stage, test_info.fresh_dir_env(), fs, None)?
            .is_some()
        {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "{} declares `fresh_dir_env`, which only Buck2's own test execution provides",
                test_target
            ));
        }

        let execution_request = Self::create_command_execution_request(
            &mut self.dice.dupe().ctx(),
            cwd,
            expanded_cmd,
            expanded_env,
            ensured_inputs,
            declared_outputs,
            argsfile_inputs,
            fs,
            None,
            None,
            None,
            vec![],
            worker,
            test_executor.re_dynamic_image(),
            test_executor.meta_internal_extra_params(),
            disable_local_network_isolation,
            None,
        )
        .await?;

        let materializer = self.dice.per_transaction_data().get_materializer();
        let blocking_executor = self.dice.ctx().get_blocking_executor();
        let re_use_case = invocation_re_use_case(&self.dice.ctx());

        // This prepares a command that the test runner executes out of process after this
        // returns. Tests buck2 runs itself go through the local executor, which holds its leases
        // across the run.
        let materialized_inputs = materialize_inputs(
            fs,
            materializer,
            &execution_request,
            self.dice.global_data().get_digest_config(),
            re_use_case,
            output_paths(fs, &execution_request)?,
        )
        .await?;

        prep_scratch_path(&materialized_inputs.scratch, fs).await?;

        create_output_dirs(
            fs,
            &execution_request,
            blocking_executor,
            self.cancellations,
        )
        .await?;
        // FIXME(materializer): The runner writes the outputs after this returns, so nothing
        // here can report them: what it writes is untracked content under buck-out that no
        // core serves, keeps or cleans. Closing that needs the runner to report what it wrote,
        // or buck2 to collect it once the runner is done with the command.
        if let Some(lease) = materialized_inputs.outputs {
            lease.abandon();
        }
        let mut held = vec![materialized_inputs.lease];

        for local_resource_setup_command in setup_commands.iter() {
            let materialized_inputs = materialize_inputs(
                fs,
                materializer,
                &local_resource_setup_command.execution_request,
                self.dice.global_data().get_digest_config(),
                re_use_case,
                output_paths(fs, &local_resource_setup_command.execution_request)?,
            )
            .await?;
            let blocking_executor = self.dice.ctx().get_blocking_executor();

            prep_scratch_path(&materialized_inputs.scratch, fs).await?;

            create_output_dirs(
                fs,
                &local_resource_setup_command.execution_request,
                blocking_executor,
                self.cancellations,
            )
            .await?;
            // FIXME(materializer): As for the test command above.
            if let Some(lease) = materialized_inputs.outputs {
                lease.abandon();
            }
            held.push(materialized_inputs.lease);
        }
        self.session.hold(output_root, ReadLease::join(held));

        Ok(create_prepare_for_local_execution_result(
            fs,
            execution_request,
            setup_commands,
        ))
    }

    async fn attach_info_message(&self, message: String) -> buck2_error::Result<()> {
        self.results_channel
            .unbounded_send(Ok(ExecutorMessage::InfoMessage(message)))
            .map_err(|_| buck2_error::internal_error!("Message received after end-of-tests"))?;
        Ok(())
    }

    async fn upload_to_cas(
        &self,
        local_path: String,
        ttl_seconds: i64,
        use_case: String,
    ) -> buck2_error::Result<CasDigest> {
        let digest_config = self.dice.global_data().get_digest_config();
        let re_digest = self
            .re_client
            .upload_local_file(&local_path, digest_config, ttl_seconds, &use_case)
            .await?;
        Ok(CasDigest {
            hash: re_digest.hash,
            size_bytes: re_digest.size_in_bytes,
        })
    }
}
#[derive(Allocative, Clone)]
struct ExecuteData {
    pub stdout: ExecutionStream,
    pub stderr: ExecutionStream,
    pub status: ExecutionStatus,
    pub timing: CommandExecutionMetadata,
    pub execution_kind: Option<CommandExecutionKind>,
    pub outputs: Vec<(BuckOutTestPath, ArtifactValue)>,
    pub command_execution: Option<buck2_data::CommandExecution>,
    /// Local case reuse or captured success for the internal runner; empty otherwise.
    pub local_case: LocalCaseOutcome,
    #[allocative(skip)]
    pub waiting_data: WaitingData,
}

impl BuckTestOrchestrator<'_> {
    /// The explicitly configured private root for local case results, if any.
    async fn local_case_result_cache_root(
        dice: &mut DiceComputations<'_>,
    ) -> buck2_error::Result<Option<String>> {
        let cell_resolver = dice.get_cell_resolver().await?;
        Ok(dice
            .get_legacy_config_property(
                cell_resolver.root_cell(),
                BuckconfigKeyRef {
                    section: "test",
                    property: "local_case_result_cache_root",
                },
            )
            .await?
            .map(|root| root.to_string()))
    }

    /// The explicitly configured private root for owned per-case scratch paths, if any.
    async fn local_case_scratch_root(
        dice: &mut DiceComputations<'_>,
    ) -> buck2_error::Result<Option<String>> {
        let cell_resolver = dice.get_cell_resolver().await?;
        Ok(dice
            .get_legacy_config_property(
                cell_resolver.root_cell(),
                BuckconfigKeyRef {
                    section: "test",
                    property: "local_case_scratch_root",
                },
            )
            .await?
            .map(|root| root.to_string()))
    }

    fn report_local_case_gap(
        test_target: &ConfiguredProvidersLabel,
        stage: &TestStage,
        gap: LocalCaseGap,
    ) {
        tracing::warn!(
            "Local case result reuse unavailable for {} ({}): {}; the case executes normally",
            test_target,
            stage,
            gap
        );
    }

    /// Serve a constrained case from the local store, or execute it and capture its success
    /// for confirmation by the internal runner.
    ///
    /// A hit never reaches `exec_cmd`, so no test child starts. A key or store failure is the
    /// case's error and starts no child. The case's launch was already checked by
    /// `check_local_case_launch`.
    async fn execute_local_case(
        local_case: ConstrainedCase,
        executor: &CommandExecutor,
        manager: CommandExecutionManager,
        prepared_command: &PreparedCommand<'_, '_>,
        cancellation: &CancellationContext,
        project_fs: &ProjectRoot,
        current_attempt: String,
        fresh_dirs: Option<&FreshDirs>,
    ) -> (CommandExecutionResult, LocalCaseOutcome) {
        let request = prepared_command.request;
        let action_digest = prepared_command.prepared_action.digest();
        let working_directory = project_fs.resolve(request.working_directory());
        let key = match local_case.key(
            request,
            PreparedFacts {
                action_digest: &action_digest,
                working_directory: &working_directory,
                network_access: format!(
                    "{:?}; disable_local_network_isolation={}",
                    prepared_command.prepared_action.network_access,
                    request.disable_local_network_isolation()
                ),
                digest_config: prepared_command.digest_config.cas_digest_config(),
            },
        ) {
            Ok(key) => key,
            Err(gap) => {
                return (
                    manager.error(
                        "local_case_result_key",
                        buck2_error::buck2_error!(
                            buck2_error::ErrorTag::Environment,
                            "Local case result key could not be built for an admitted case: {gap}"
                        ),
                    ),
                    LocalCaseOutcome::default(),
                );
            }
        };

        let query = buck2_data::LocalTestResultCacheQuery {
            action_digest: key.action_digest().to_owned(),
            cache_key: key.digest().to_owned(),
            current_attempt: current_attempt.clone(),
        };
        let reuse = local_case.reuse();
        // Opens the store, waits for the key's lease without holding a blocking thread, and
        // either returns a verified hit or durably begins the next generation; the lease then
        // stays with the returned hit or pending attempt.
        let lookup = executor_stage_async(
            query,
            LocalCaseStart::take(local_case.root(), &key, reuse, &current_attempt),
        )
        .await;
        let lookup = match lookup {
            Ok(start) => start,
            Err(e) => {
                return (
                    manager.error("local_case_result_store", e),
                    LocalCaseOutcome::default(),
                );
            }
        };
        let pending = match lookup {
            LocalCaseStart::Hit(reused) => {
                let provenance = reused.provenance().clone();
                let execution_kind = CommandExecutionKind::LocalTestResultCache {
                    digest: action_digest.dupe(),
                    cache_key: provenance.cache_key.clone(),
                    origin_attempt: provenance.origin_attempt.clone(),
                    entry_manifest: provenance.entry_manifest.clone(),
                    current_attempt: current_attempt.clone(),
                };
                let std_streams = CommandStdStreams::Local {
                    stdout: reused.stdout().to_vec(),
                    stderr: reused.stderr().to_vec(),
                };
                let result = executor_stage_async(
                    buck2_data::LocalTestResultCache {
                        action_digest: provenance.action_digest,
                        cache_key: provenance.cache_key,
                        origin_attempt: provenance.origin_attempt,
                        entry_manifest: provenance.entry_manifest,
                        current_attempt,
                    },
                    async move {
                        manager.claim().await.success(
                            execution_kind,
                            BuckMutInsertionMap::default(),
                            std_streams,
                            reuse_timing(),
                        )
                    },
                )
                .await;
                return (
                    result,
                    LocalCaseOutcome::new(LocalCaseResult::Reused(reused)),
                );
            }
            LocalCaseStart::Begun(pending) => pending,
        };
        // Under the key's lease, and before the executor removes the scratch path. A holder of a
        // scratch lease never waits for a key's lease, so the two cannot deadlock.
        let pending = match fresh_dirs {
            Some(fresh_dirs) => pending.holding(fresh_dirs.lease().await),
            None => pending,
        };

        // Execution and settlement form one critical section: a cancelled case is not dropped
        // between them, so its child is killed and the streams it already wrote are settled into
        // the generation. A local exit-0 success is captured for confirmation; anything else,
        // including a cancelled child, records the generation as unsuccessful with the exact raw
        // streams the executor returned.
        cancellation
            .critical_section(|| async move {
                let result = executor
                    .exec_cmd(manager, prepared_command, cancellation)
                    .await;
                let outcome = settle_execution(pending, &result.report).await;
                (result, outcome)
            })
            .await
    }

    /// Materialize the exact values of the input-tree executables a constrained case launches,
    /// hold them for reading, and check their `#!` interpreters in those values. A case found
    /// ineligible here is given up; the caller restores its legacy request, exactly as for a case
    /// found ineligible before it was constrained.
    ///
    /// The values are those the action's input tree binds, which the executor materializes at
    /// the same paths before it starts the command: the check reads what will run, never what
    /// an earlier artifact or nothing left at the path. A path-only request would not do: the
    /// disk-state materializer has no value to make for a bare path and leaves the path alone.
    async fn check_local_case_launch(
        test_target_label: &ConfiguredProvidersLabel,
        stage: &TestStage,
        materializer: &dyn Materializer,
        re_use_case: RemoteExecutorUseCase,
        local_case: ConstrainedCase,
    ) -> LaunchCheck {
        let launch = match local_case
            .launch()
            .clone()
            .materialize(materializer, re_use_case)
            .await
        {
            Ok(Ok(launch)) => launch,
            Ok(Err(e)) => {
                return LaunchCheck::Failed("local_case_launch_materialization", e.into());
            }
            Err(e) => return LaunchCheck::Failed("local_case_launch_materialization", e),
        };
        // The lease travels with the check and is released when it ends, before the action is
        // executed (`MaterializedLaunch`).
        match tokio::task::spawn_blocking(move || launch.verify()).await {
            Ok(Ok(())) => LaunchCheck::Eligible(local_case),
            Ok(Err(why)) => {
                Self::report_local_case_gap(
                    test_target_label,
                    stage,
                    LocalCaseGap::UnboundCommandFile(why),
                );
                LaunchCheck::Ineligible(local_case)
            }
            Err(e) => LaunchCheck::Failed(
                "local_case_launch_check",
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Environment,
                    "Local case launch check did not complete: {e}"
                ),
            ),
        }
    }

    fn executor_preference(
        opts: TestSessionOptions,
        test_supports_re: bool,
    ) -> buck2_error::Result<ExecutorPreference> {
        let mut executor_preference = ExecutorPreference::Default;

        if !opts.allow_re {
            // We don't ban RE (we only prefer not to use it) if the session doesn't allow it, so
            // that executor overrides or default executor can still route executions to RE.
            executor_preference = executor_preference.and(ExecutorPreference::LocalPreferred)?;
        }

        if !test_supports_re {
            // But if the test doesn't support RE at all, then we ban it.
            executor_preference = executor_preference.and(ExecutorPreference::LocalRequired)?;
        }

        Ok(executor_preference)
    }

    /// Core request execution logic.
    async fn execute_request(
        dice: &mut DiceComputations<'_>,
        cancellation: &CancellationContext,
        test_target_label: &ConfiguredProvidersLabel,
        stage: &TestStage,
        executor: &CommandExecutor,
        mut request: CommandExecutionRequest,
        liveliness_observer: Arc<dyn LivelinessObserver>,
        re_cache_enabled: bool,
        supports_test_execution_caching: bool,
        local_case: Option<ConstrainedCase>,
        fresh_dirs: Option<FreshDirs>,
        waiting_data: WaitingData,
    ) -> Result<ExecuteData, ExecuteError> {
        let events = dice.per_transaction_data().get_dispatcher().dupe();
        let project_fs = executor.fs().fs().dupe();
        let manager = CommandExecutionManager::new(
            Box::new(MutexClaimManager::new()),
            events.dupe(),
            liveliness_observer.dupe(),
            waiting_data,
        );
        let digest_config = dice.global_data().get_digest_config();

        let test_target = TestTarget {
            target: test_target_label.target(),
            action_key_suffix: create_action_key_suffix(stage),
        };

        // A constrained case whose launch turns out ineligible is prepared and executed with its
        // legacy request, as if reuse had not been requested.
        let (local_case, launch_failure) = match local_case {
            None => (None, None),
            Some(local_case) => {
                // Only a constrained case needs the materializer: an ordinary request runs
                // without one, as upstream's orchestrator does.
                let materializer = dice.per_transaction_data().get_materializer_handle();
                let re_use_case = invocation_re_use_case(dice);
                match Self::check_local_case_launch(
                    test_target_label,
                    stage,
                    &*materializer,
                    re_use_case,
                    local_case,
                )
                .await
                {
                    LaunchCheck::Eligible(local_case) => (Some(local_case), None),
                    LaunchCheck::Ineligible(local_case) => {
                        request = local_case.into_legacy_request(request);
                        (None, None)
                    }
                    LaunchCheck::Failed(what, e) => (None, Some((what, e))),
                }
            }
        };

        // For test execution, we currently do not do any cache queries

        let prepared_action = match executor.prepare_action(&request, digest_config, false) {
            Ok(prepared_action) => prepared_action,
            Err(e) => return Err(ExecuteError::Error(e)),
        };
        let prepared_command = PreparedCommand {
            target: &test_target as _,
            request: &request,
            prepared_action: &prepared_action,
            digest_config,
        };

        let mut local_case_outcome = LocalCaseOutcome::default();

        // instrument execution with a span.
        // TODO(brasselsprouts): migrate this into the executor to get better accuracy.
        let command_exec_result = match stage {
            TestStage::Listing { suite, cacheable } => {
                let start = TestDiscoveryStart {
                    target_label: Some(test_target.target.as_proto()),
                    suite_name: suite.clone(),
                };
                let (result, cached) = events
                    .span_async(start, async move {
                        let (result, cached) = if *cacheable {
                            match executor
                                .action_cache(manager, &prepared_command, cancellation)
                                .await
                            {
                                ControlFlow::Continue(manager) => {
                                    let result = executor
                                        .exec_cmd(manager, &prepared_command, cancellation)
                                        .await;
                                    (result, false)
                                }
                                ControlFlow::Break(result) => (result, true),
                            }
                        } else {
                            let result = executor
                                .exec_cmd(manager, &prepared_command, cancellation)
                                .await;
                            (result, false)
                        };
                        let end = TestDiscoveryEnd {
                            suite_name: suite.clone(),
                            target_label: Some(test_target.target.as_proto()),
                            command_report: Some(
                                result
                                    .report
                                    .to_command_execution_proto(true, true, false)
                                    .await,
                            ),
                            command_host_sharing_requirements: host_sharing_requirements_to_grpc(
                                prepared_command.request.host_sharing_requirements().clone(),
                            )
                            .ok(),
                            re_cache_enabled: *cacheable && re_cache_enabled,
                        };
                        ((result, cached), end)
                    })
                    .await;
                if !cached && *cacheable {
                    let info = CacheUploadInfo {
                        target: &test_target as _,
                        digest_config,
                        mergebase: &None,
                        re_platform: executor.re_platform(),
                        allow_cache_upload: true,
                    };
                    let _result = match executor
                        .cache_upload(
                            &info,
                            &result,
                            None,
                            None,
                            &prepared_action.action_and_blobs,
                        )
                        .await
                    {
                        Ok(result) => result,
                        Err(e) => return Err(ExecuteError::Error(e)),
                    };
                }
                result
            }
            TestStage::Testing {
                suite, testcases, ..
            } => {
                let test_suite = Some(TestSuite {
                    suite_name: suite.clone(),
                    test_names: testcases.clone(),
                    target_label: Some(test_target.target.as_proto()),
                });
                let start = TestRunStart {
                    suite: test_suite.clone(),
                };
                let local_case_outcome = &mut local_case_outcome;
                let current_attempt = events.trace_id().to_string();
                events
                    .span_async(start, async move {
                        // An untracked execution holds the fresh-directory scratch path until its
                        // command has run; a tracked case takes it under its key's lease and
                        // keeps it with its result.
                        let result = if let Some((what, e)) = launch_failure {
                            manager.error(what, e)
                        } else if let Some(local_case) = local_case {
                            let (result, outcome) = Self::execute_local_case(
                                local_case,
                                executor,
                                manager,
                                &prepared_command,
                                cancellation,
                                &project_fs,
                                current_attempt,
                                fresh_dirs.as_ref(),
                            )
                            .await;
                            *local_case_outcome = outcome;
                            result
                        } else if let Some(fresh_dirs) = &fresh_dirs {
                            // This runs as its own task with a real cancellation context (see
                            // `execute2`): cancelled, the executor stops and reaps the child inside
                            // the critical section, and only then is the lease released.
                            let scratch = fresh_dirs.lease().await;
                            let prepared_command = &prepared_command;
                            cancellation
                                .critical_section(|| async move {
                                    let result = executor
                                        .exec_cmd(manager, prepared_command, cancellation)
                                        .await;
                                    drop(scratch);
                                    result
                                })
                                .await
                        } else if supports_test_execution_caching {
                            match executor
                                .action_cache(manager, &prepared_command, cancellation)
                                .await
                            {
                                ControlFlow::Continue(manager) => {
                                    executor
                                        .exec_cmd(manager, &prepared_command, cancellation)
                                        .await
                                }
                                ControlFlow::Break(result) => result,
                            }
                        } else {
                            executor
                                .exec_cmd(manager, &prepared_command, cancellation)
                                .await
                        };
                        let end = TestRunEnd {
                            suite: test_suite,
                            command_report: Some(
                                result
                                    .report
                                    .to_command_execution_proto(true, true, false)
                                    .await,
                            ),
                            command_host_sharing_requirements: host_sharing_requirements_to_grpc(
                                prepared_command.request.host_sharing_requirements().clone(),
                            )
                            .ok(),
                        };
                        (result, end)
                    })
                    .await
            }
        };
        let command_execution = Some(
            command_exec_result
                .report
                .to_command_execution_proto(false, false, false)
                .await,
        );

        let CommandExecutionResult {
            outputs,
            report:
                CommandExecutionReport {
                    std_streams,
                    exit_code,
                    status,
                    timing,
                    ..
                },
            waiting_data,
            ..
        } = command_exec_result;

        let outputs = outputs
            .into_iter()
            .filter_map(|(output, artifact)| Some((output.into_test_path()?, artifact)))
            .collect();

        let std_streams = std_streams
            .into_bytes()
            .await
            .buck_error_context("Error accessing test output")?;
        let stdout = ExecutionStream::Inline(std_streams.stdout);
        let stderr = ExecutionStream::Inline(std_streams.stderr);

        // If we are shutting down, we may have terminated executions and caused
        // the outcomes we are reporting (typically w/ a worker failure).
        Self::require_alive(liveliness_observer.dupe()).await?;

        Ok(match status {
            CommandExecutionStatus::Success { execution_kind } => ExecuteData {
                stdout,
                stderr,
                status: ExecutionStatus::Finished {
                    exitcode: exit_code.unwrap_or(0),
                },
                timing,
                execution_kind: Some(execution_kind),
                outputs,
                command_execution,
                local_case: local_case_outcome,
                waiting_data,
            },
            CommandExecutionStatus::Failure { execution_kind, .. }
            | CommandExecutionStatus::WorkerFailure { execution_kind } => ExecuteData {
                stdout,
                stderr,
                status: ExecutionStatus::Finished {
                    exitcode: failure_exit_code(exit_code),
                },
                timing,
                execution_kind: Some(execution_kind),
                outputs,
                command_execution,
                local_case: local_case_outcome,
                waiting_data,
            },
            CommandExecutionStatus::TimedOut {
                duration,
                execution_kind,
            } => ExecuteData {
                stdout,
                stderr,
                status: ExecutionStatus::TimedOut { duration },
                timing,
                execution_kind: Some(execution_kind),
                outputs,
                command_execution,
                local_case: local_case_outcome,
                waiting_data,
            },
            CommandExecutionStatus::Error {
                error,
                execution_kind,
                ..
            } => ExecuteData {
                stdout: ExecutionStream::Inline(Default::default()),
                stderr: ExecutionStream::Inline(format!("{error:?}").into_bytes()),
                status: ExecutionStatus::Finished {
                    exitcode: failure_exit_code(exit_code),
                },
                timing,
                execution_kind,
                outputs,
                command_execution,
                local_case: local_case_outcome,
                waiting_data,
            },
            CommandExecutionStatus::Cancelled {
                execution_kind: _,
                reason,
            } => {
                let reason = reason.map(|reason| match reason {
                    CommandCancellationReason::NotSpecified => CancellationReason::NotSpecified,
                    CommandCancellationReason::ReQueueTimeout => CancellationReason::ReQueueTimeout,
                });
                return Err(ExecuteError::Cancelled(Cancelled { reason }));
            }
        })
    }

    fn executor_config_with_remote_cache_override<'a>(
        test_target_node: &'a ConfiguredTargetNode,
        executor_override: Option<&'a CommandExecutorConfig>,
        stage: &TestStage,
        supports_test_execution_caching: bool,
    ) -> buck2_error::Result<Cow<'a, CommandExecutorConfig>> {
        let executor_config = match executor_override {
            Some(o) => o,
            None => test_target_node
                .execution_platform_resolution()
                .executor_config()
                .buck_error_context("Error accessing executor config")?,
        };

        if let TestStage::Listing { .. } = &stage {
            return Ok(Cow::Borrowed(executor_config));
        }

        if supports_test_execution_caching {
            return Ok(Cow::Borrowed(executor_config));
        }

        match &executor_config.executor {
            Executor::RemoteEnabled(options) if options.remote_cache_enabled => {
                let mut exec_options = options.clone();
                exec_options.remote_cache_enabled = false;
                let executor_config = CommandExecutorConfig {
                    executor: Executor::RemoteEnabled(exec_options),
                    options: executor_config.options.dupe(),
                };
                Ok(Cow::Owned(executor_config))
            }
            Executor::Local(_) | Executor::RemoteEnabled(_) | Executor::None => {
                Ok(Cow::Borrowed(executor_config))
            }
        }
    }

    /// Which of the executor factory's action cache checker and cache uploader one test command
    /// keeps, in that order. A listing keeps both. A test command never uploads, and checks the
    /// action cache only when it supports test execution caching. An admitted local-workspace
    /// test reads its live workspace, so neither its listing nor its test command ever checks or
    /// uploads a cache.
    fn test_command_cache_paths(
        stage: &TestStage,
        supports_test_execution_caching: bool,
        local_execution: LocalExecution,
    ) -> (TestCachePath, TestCachePath) {
        match (local_execution, stage) {
            (LocalExecution::AdmittedLocalWorkspaceTest, _) => {
                (TestCachePath::NoOp, TestCachePath::NoOp)
            }
            (LocalExecution::ByStrategy | LocalExecution::AdmittedInternalTest, stage) => {
                match stage {
                    TestStage::Listing { .. } => (TestCachePath::Provided, TestCachePath::Provided),
                    TestStage::Testing { .. } => (
                        if supports_test_execution_caching {
                            TestCachePath::Provided
                        } else {
                            TestCachePath::NoOp
                        },
                        // We never upload local test executions
                        TestCachePath::NoOp,
                    ),
                }
            }
        }
    }

    fn get_command_executor(
        dice: &mut DiceComputations<'_>,
        fs: &ArtifactFs,
        executor_config: &CommandExecutorConfig,
        stage: &TestStage,
        supports_test_execution_caching: bool,
        local_execution: LocalExecution,
    ) -> buck2_error::Result<CommandExecutor> {
        let CommandExecutorResponse {
            executor,
            platform,
            action_cache_checker,
            remote_dep_file_cache_checker: _,
            cache_uploader,
            output_trees_download_config: _,
            write_action_cache: _,
        } = dice.get_command_executor_from_dice(fs, executor_config, local_execution)?;

        let (checker_path, uploader_path) =
            Self::test_command_cache_paths(stage, supports_test_execution_caching, local_execution);
        let action_cache_checker = match checker_path {
            TestCachePath::Provided => action_cache_checker,
            TestCachePath::NoOp => Arc::new(NoOpCommandOptionalExecutor {}) as _,
        };
        let cache_uploader = match uploader_path {
            TestCachePath::Provided => cache_uploader,
            TestCachePath::NoOp => Arc::new(NoOpCacheUploader {}) as _,
        };

        let executor = CommandExecutor::new(
            executor,
            action_cache_checker,
            Arc::new(NoOpCommandOptionalExecutor {}),
            cache_uploader,
            None,
            fs.dupe(),
            executor_config.options,
            platform,
        );
        Ok(executor)
    }

    fn get_local_executor(
        dice: &mut DiceComputations<'_>,
        fs: &ArtifactFs,
    ) -> buck2_error::Result<CommandExecutor> {
        let executor_config = CommandExecutorConfig {
            executor: Executor::Local(LocalExecutorOptions::default()),
            options: CommandGenerationOptions {
                path_separator: PathSeparatorKind::system_default(),
                output_paths_behavior: Default::default(),
                use_bazel_protocol_remote_persistent_workers: false,
                network_access: None,
            },
        };
        let CommandExecutorResponse {
            executor,
            platform,
            action_cache_checker: _,
            remote_dep_file_cache_checker: _,
            cache_uploader: _,
            output_trees_download_config: _,
            write_action_cache: _,
        } =
            dice.get_command_executor_from_dice(fs, &executor_config, LocalExecution::ByStrategy)?;
        let executor = CommandExecutor::new(
            executor,
            Arc::new(NoOpCommandOptionalExecutor {}),
            Arc::new(NoOpCommandOptionalExecutor {}),
            Arc::new(NoOpCacheUploader {}),
            None,
            fs.dupe(),
            executor_config.options,
            platform,
        );
        Ok(executor)
    }

    async fn get_test_info(
        dice: &mut DiceComputations<'_>,
        test_target: &ConfiguredProvidersLabel,
        internal_runner_config: &InternalRunnerConfig,
    ) -> buck2_error::Result<OwnedTestInfo> {
        let providers = dice
            .get_providers(test_target)
            .await?
            .require_compatible()?;
        OwnedTestInfo::select(&providers, internal_runner_config)?.ok_or_else(|| {
            internal_error!(
                "Test executable requires ExternalRunnerTestInfo or InternalRunnerTestInfo"
            )
        })
    }

    /// One closed `[test]` local-admission boolean of the root cell. Absent means `false`; any
    /// value other than `true` or `false` is refused.
    fn parse_local_admission(property: &str, value: Option<&str>) -> buck2_error::Result<bool> {
        match value {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(other) => Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "[test] {} must be `true` or `false`, not `{}`",
                property,
                other
            )),
        }
    }

    async fn local_admission(
        dice: &mut DiceComputations<'_>,
        property: &'static str,
    ) -> buck2_error::Result<bool> {
        let cell_resolver = dice.get_cell_resolver().await?;
        let value = dice
            .get_legacy_config_property(
                cell_resolver.root_cell(),
                BuckconfigKeyRef {
                    section: "test",
                    property,
                },
            )
            .await?;
        Self::parse_local_admission(property, value.as_ref().map(|v| v.to_string()).as_deref())
    }

    /// The two independent `[test]` local admissions:
    /// - `local_internal_cases`: an `InternalRunnerTestInfo` test whose provider explicitly
    ///   selects the local executor may use it under `--remote-only`;
    /// - `local_workspace_tests`: an `ExternalRunnerTestInfo` test whose provider explicitly
    ///   selects the local executor and that does not support test execution caching may use it,
    ///   uncached, under `--remote-only`.
    async fn local_test_admissions(
        dice: &mut DiceComputations<'_>,
    ) -> buck2_error::Result<LocalTestAdmissions> {
        Ok(LocalTestAdmissions {
            internal_cases: Self::local_admission(dice, "local_internal_cases").await?,
            workspace_tests: Self::local_admission(dice, "local_workspace_tests").await?,
        })
    }

    /// The local-execution admission for one listing or test command. The provider must have
    /// explicitly selected the local executor (its `default_executor` or the named override) with
    /// an `ExecutorConfigOrigin::Declared` value; a platform-derived executor is never admitted,
    /// whether it is the fallback (`None`) or one the test info recorded from the execution
    /// platform of its execution group (`ExecutorConfigOrigin::ExecutionGroupPlatform`). Then:
    /// - an `InternalRunnerTestInfo` test is admitted under `[test] local_internal_cases`;
    /// - an `ExternalRunnerTestInfo` test that does not support test execution caching is
    ///   admitted under `[test] local_workspace_tests`, as a local-workspace test, never as an
    ///   Internal one.
    ///
    /// Neither option admits the other's provider kind, and an option alone admits nothing.
    /// `supports_test_execution_caching` is true when the provider declares test execution
    /// caching or this command would use it; such an External test is refused, whatever the
    /// invocation's own caching flags.
    fn local_execution_for_test(
        kind: TestProviderKind,
        provider_executor: Option<&StarlarkCommandExecutorConfig>,
        supports_test_execution_caching: bool,
        admissions: LocalTestAdmissions,
    ) -> LocalExecution {
        let declared_executor = match provider_executor {
            Some(config) => match config.origin() {
                ExecutorConfigOrigin::Declared => &*config.0,
                ExecutorConfigOrigin::ExecutionGroupPlatform => return LocalExecution::ByStrategy,
            },
            None => return LocalExecution::ByStrategy,
        };
        let CommandExecutorConfig {
            executor: Executor::Local(_),
            ..
        } = declared_executor
        else {
            return LocalExecution::ByStrategy;
        };
        match kind {
            TestProviderKind::Internal if admissions.internal_cases => {
                LocalExecution::AdmittedInternalTest
            }
            TestProviderKind::External
                if admissions.workspace_tests && !supports_test_execution_caching =>
            {
                LocalExecution::AdmittedLocalWorkspaceTest
            }
            TestProviderKind::Internal | TestProviderKind::External => LocalExecution::ByStrategy,
        }
    }

    /// The executor the provider selected for one listing or test command (the named
    /// `executor_override`, else for a listing the `listing` override or the default, else the
    /// default), and that command's local-execution admission. The default may have been
    /// recorded from the test info's execution group; it is still the command's executor, but its
    /// origin keeps it from being admitted locally. Both executor construction and
    /// the choice of DICE result reuse use this one resolution, so they cannot disagree about
    /// which commands are admitted local-workspace tests.
    async fn provider_executor_admission<'t>(
        dice: &mut DiceComputations<'_>,
        test_info: &'t OwnedTestInfo,
        executor_override: Option<&ExecutorConfigOverride>,
        stage: &TestStage,
        supports_test_execution_caching: bool,
    ) -> buck2_error::Result<(Option<&'t CommandExecutorConfig>, LocalExecution)> {
        let resolved_executor_override = match executor_override {
            Some(executor_override) => Some(
                test_info
                    .executor_override(&executor_override.name)
                    .ok_or_else(|| {
                        internal_error!("The `executor_override` provided does not exist")
                    })
                    .with_buck_error_context(|| {
                        format!(
                            "Error processing `executor_override`: `{}`",
                            executor_override.name
                        )
                    })?,
            ),
            None => match stage {
                TestStage::Listing { .. } if test_info.has_executor_overrides() => test_info
                    .executor_override("listing")
                    .or(test_info.default_executor()),
                _ => test_info.default_executor(),
            },
        };

        let local_execution = Self::local_execution_for_test(
            test_info.kind(),
            resolved_executor_override,
            test_info.supports_test_execution_caching() || supports_test_execution_caching,
            Self::local_test_admissions(dice).await?,
        );
        let provider_executor = resolved_executor_override.map(|o| &*o.0);
        Ok((provider_executor, local_execution))
    }

    async fn get_test_executor(
        dice: &mut DiceComputations<'_>,
        test_target: &ConfiguredProvidersLabel,
        test_info: &OwnedTestInfo,
        executor_override: Option<Arc<ExecutorConfigOverride>>,
        fs: &ArtifactFs,
        stage: &TestStage,
        supports_test_execution_caching: bool,
    ) -> buck2_error::Result<TestExecutor> {
        // NOTE: get_providers() implicitly calls this already but it's not the end of the world
        // since this will get cached in DICE.
        let node = dice
            .get_configured_target_node(test_target.target())
            .await
            .require_compatible()?;

        // Local admission considers only an executor the provider explicitly selected; an
        // executor resolved from the test's execution group is never admitted locally, whether
        // the test info recorded it during analysis (its origin says so) or it is resolved below.
        let (provider_executor, local_execution) = Self::provider_executor_admission(
            dice,
            test_info,
            executor_override.as_deref(),
            stage,
            supports_test_execution_caching,
        )
        .await?;

        let execution_group_executor_config = match (provider_executor, test_info.execution_group()?) {
            (None, None | Some(ExecutionGroup::Default))
                if !node
                    .execution_platform_resolution()
                    .default_execution_group_declared() =>
            {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "The test uses the default execution group, which the rule of `{test_target}` does not declare (the test info may come from another rule that created it). Set `exec_group` or `default_executor` where the test info is created, or add `DefaultExecGroup` to `exec_groups` of the rule of `{test_target}`"
                ));
            }
            (None, Some(group)) => Some(
                node.execution_platform_resolution()
                    .executor_config_for_group(&group)
                    .with_buck_error_context(|| {
                        format!(
                            "The test info of `{test_target}` names execution group `{group}`, but no executor was recorded for it when the test info was created, so it is resolved against `{test_target}`"
                        )
                    })?
                    .dupe(),
            ),
            _ => None,
        };

        let executor_config = Self::executor_config_with_remote_cache_override(
            node,
            provider_executor.or(execution_group_executor_config.as_deref()),
            stage,
            supports_test_execution_caching,
        )?;

        let executor = Self::get_command_executor(
            dice,
            fs,
            &executor_config,
            stage,
            supports_test_execution_caching,
            local_execution,
        )
        .buck_error_context("Error constructing CommandExecutor")?;

        Ok(TestExecutor {
            test_executor: executor,
            executor_config: executor_config.into_owned(),
        })
    }

    /// Replaces each argument carrying argsfile contents with `@<path>`, and returns the files as
    /// action inputs.
    ///
    /// Writing them here, before an executor is chosen, makes each file an input of the action,
    /// so it is hashed into the action digest and reaches remote execution, local execution and
    /// workers by the same path as any other input.
    async fn materialize_argsfiles<'a>(
        dice: &mut DiceComputations<'_>,
        test_target: &ConfiguredProvidersLabel,
        cmd: Cow<'a, [ArgValue]>,
        fs: &ArtifactFs,
    ) -> buck2_error::Result<(Vec<ExpandableArg>, Vec<CommandExecutionInput>)> {
        let digest_config = dice.global_data().get_digest_config();
        let materializer = dice.per_transaction_data().get_materializer();

        let mut inputs = Vec::new();
        let mut expanded = Vec::with_capacity(cmd.len());
        for arg in cmd.into_owned() {
            let ArgValueContent::ArgsfileContents(contents) = arg.content else {
                expanded.push(ExpandableArg::Value(arg));
                continue;
            };
            if arg.format.is_some() {
                return Err(internal_error!(
                    "Argsfile contents do not support a format string"
                ));
            }

            let digest =
                TrackedFileDigest::from_content(&contents, digest_config.cas_digest_config());
            let content_hash = ContentBasedPathHash::new(digest.raw_digest().as_bytes())?;
            let path = BuildArtifactPath::new(
                BaseDeferredKey::TargetLabel(test_target.target().dupe()),
                ForwardRelativePathBuf::unchecked_new(format!(
                    "__test_argsfile_{}__",
                    inputs.len()
                )),
                BuckOutPathKind::ContentHash,
            );
            let project_rel_path = fs
                .buck_out_path_resolver()
                .resolve_gen(&path, Some(&content_hash))?;

            let request = WriteRequest {
                path: project_rel_path.clone(),
                content: contents,
                is_executable: false,
                path_kind: path.path_resolution_method(),
            };
            let written = if materializer.is_disk_state() {
                write_locally(
                    materializer,
                    dice.get_blocking_executor(),
                    fs.fs(),
                    digest_config,
                    request,
                )
                .await
                .map(|_| ())
            } else {
                materializer
                    .declare_write(Box::new(move || Ok(vec![request])))
                    .await
                    .map(|_| ())
            };
            written.buck_error_context("Failed to write test argsfile")?;

            expanded.push(ExpandableArg::Argsfile(project_rel_path));
            inputs.push(CommandExecutionInput::ActionMetadata(ActionMetadataBlob {
                digest,
                path,
                content_hash,
            }));
        }

        Ok((expanded, inputs))
    }

    async fn expand_test_executable<'a>(
        dice: &mut DiceComputations<'_>,
        test_target: &ConfiguredProvidersLabel,
        test_info: &OwnedTestInfo,
        cmd: Cow<'a, [ArgValue]>,
        env: Cow<'a, BuckFixedSortedMap<String, ArgValue>>,
        pre_create_dirs: Cow<'a, [DeclaredOutput]>,
        executor_fs: &ExecutorFs<'_>,
        stage: &TestStage,
        opts: TestSessionOptions,
    ) -> buck2_error::Result<ExpandedTestExecutable> {
        let (cmd, argsfile_inputs) =
            Self::materialize_argsfiles(dice, test_target, cmd, executor_fs.fs()).await?;

        let output_root = resolve_output_root(dice, test_target, stage).await?;

        let mut declared_outputs =
            BuckMutInsertionMap::<BuckOutTestPath, OutputCreationBehavior>::new();

        let mut supports_re = true;

        let cwd;
        let (expanded_cmd, expanded_env, ensured_inputs, expanded_worker) = {
            cwd = if test_info.run_from_project_root() || opts.force_run_from_project_root {
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("".to_owned()))
            } else {
                supports_re = false;
                // For compatibility with v1,
                let cell_resolver = dice.get_cell_resolver().await?;
                let cell = cell_resolver.get(test_target.target().pkg().cell_name())?;
                cell.path().to_buf()
            };

            let expander = Execute2RequestExpander {
                test_info,
                stage,
                output_root: &output_root,
                declared_outputs: &mut declared_outputs,
                fs: executor_fs,
                cmd,
                env,
                digest_config: dice.global_data().get_digest_config(),
            };

            let inputs = expander.get_inputs()?;
            // We already built these before reaching out to tpx, so these should already be ready.
            let ensured_inputs =
                KeepGoing::try_compute_join_all(dice, inputs, async |dice, input| {
                    let artifact_group_value = dice.ensure_artifact_group(&input).await?;
                    buck2_error::Ok((input, artifact_group_value))
                })
                .await?;

            let (expanded_cmd, expanded_env, expanded_worker) = if test_info
                .use_project_relative_paths()
                || opts.force_use_project_relative_paths
            {
                expander.expand(&ensured_inputs, false)
            } else {
                supports_re = false;
                expander.expand(&ensured_inputs, true)
            }?;
            (expanded_cmd, expanded_env, ensured_inputs, expanded_worker)
        };

        for output in pre_create_dirs.into_owned() {
            let test_path = BuckOutTestPath::new(output_root.clone(), output.name.into());
            declared_outputs.insert(test_path, OutputCreationBehavior::Create);
        }

        Ok(ExpandedTestExecutable {
            cwd: cwd.as_project_relative_path().to_buf(),
            cmd: expanded_cmd,
            env: expanded_env,
            ensured_inputs,
            declared_outputs,
            argsfile_inputs,
            supports_re,
            worker: expanded_worker,
        })
    }

    async fn create_command_execution_request(
        dice: &mut DiceComputations<'_>,
        cwd: ProjectRelativePathBuf,
        cmd: Vec<String>,
        env: BuckFixedSortedMap<String, String>,
        ensured_inputs: Vec<(ArtifactGroup, ArtifactGroupValues)>,
        declared_outputs: BuckMutInsertionMap<BuckOutTestPath, OutputCreationBehavior>,
        argsfile_inputs: Vec<CommandExecutionInput>,
        fs: &ArtifactFs,
        timeout: Option<Duration>,
        host_sharing_requirements: Option<Arc<HostSharingRequirements>>,
        executor_preference: Option<ExecutorPreference>,
        required_local_resources: Vec<LocalResourceState>,
        worker: Option<WorkerSpec>,
        re_dynamic_image: Option<RemoteExecutorCustomImage>,
        meta_internal_extra_params: Arc<MetaInternalExtraParams>,
        disable_local_network_isolation: bool,
        fresh_dirs: Option<&FreshDirs>,
    ) -> buck2_error::Result<CommandExecutionRequest> {
        let inputs = ensured_inputs
            .into_iter()
            .map(|(_, v)| CommandExecutionInput::Artifact(Box::new(v)))
            .chain(fresh_dirs.and_then(FreshDirs::input))
            .chain(argsfile_inputs)
            .collect_vec();

        // NOTE: This looks a bit awkward, that's because fbcode's rustfmt and ours slightly
        // disagree about format here...
        let outputs = declared_outputs
            .into_iter()
            .map(|(path, create)| CommandExecutionOutput::TestPath { path, create })
            .collect();
        let digest_config = dice.global_data().get_digest_config();
        let mut request = CommandExecutionRequest::new(
            vec![],
            cmd,
            CommandExecutionPaths::new(
                inputs,
                outputs,
                fs,
                digest_config,
                dice.per_transaction_data()
                    .get_run_action_knobs()
                    .action_paths_interner
                    .as_ref(),
            )?,
            env,
        );
        let has_resource_control = dice
            .per_transaction_data()
            .data
            .get::<HasResourceControl>()
            .unwrap()
            .0;
        request = request
            .with_working_directory(cwd)
            .with_local_environment_inheritance(EnvironmentInheritance::test_allowlist())
            .with_disable_miniperf(!has_resource_control)
            .with_worker(worker)
            .with_remote_execution_custom_image(re_dynamic_image)
            .with_meta_internal_extra_params(meta_internal_extra_params)
            .with_required_local_resources(required_local_resources)?
            .with_disable_local_network_isolation(disable_local_network_isolation)
            .with_scratch_dir_variables(fresh_dirs.map(FreshDirs::variables).unwrap_or_default())
            .with_owned_scratch_path(fresh_dirs.and_then(FreshDirs::owned_scratch_path))
            .with_is_test();
        if let Some(timeout) = timeout {
            request = request.with_timeout(timeout)
        }
        if let Some(host_sharing_requirements) = host_sharing_requirements {
            request = request.with_host_sharing_requirements(host_sharing_requirements.dupe());
        }
        if let Some(executor_preference) = executor_preference {
            request = request.with_executor_preference(executor_preference);
        }
        Ok(request)
    }

    async fn setup_local_resources(
        dice: &mut DiceComputations<'_>,
        cancellation: &CancellationContext,
        required_providers: Vec<(&'_ ConfiguredTargetLabel, OwnedLocalResourceInfo)>,
        executor: CommandExecutor,
        default_timeout: Duration,
        liveliness_observer: Arc<dyn LivelinessObserver>,
    ) -> Result<Vec<LocalResourceState>, ExecuteError> {
        if required_providers.is_empty() {
            return Ok(vec![]);
        }
        let setup_commands = dice
            .try_compute_join(required_providers, async |dice, provider| {
                Self::prepare_local_resource(
                    dice,
                    provider,
                    &executor.fs(),
                    &executor.executor_fs(),
                    default_timeout,
                )
                .await
            })
            .await?;

        Self::require_alive(liveliness_observer.dupe()).await?;
        let events = dice.per_transaction_data().get_dispatcher().dupe();
        let digest_config = dice.global_data().get_digest_config();

        // TODO(romanp): The code below is not optimal. We are locking the entire registry here, but we could have better concurrency.
        // For example, if different suites require different local resources and can execute in parallel, this code runs sequentially but should run in parallel.
        // An easy fix would be to introduce an RwLock instead of a mutex. In this case, suites that have the necessary resources and do not require write access
        // can be executed in parallel.
        let local_resource_state_registry = dice.get_local_resource_registry()?;
        let required_targets = setup_commands
            .iter()
            .map(|ctx| ctx.target.dupe())
            .collect::<Vec<_>>();
        let mut lock = local_resource_state_registry.0.lock().await;

        let resource_futs = setup_commands
            .into_iter()
            .filter(|ctx| !lock.contains_key(&ctx.target))
            .map(|ctx| {
                let missing_target = ctx.target.dupe();
                let setup = Self::start_local_resource(
                    events.dupe(),
                    liveliness_observer.dupe(),
                    digest_config.dupe(),
                    executor.dupe(),
                    ctx,
                    cancellation,
                );
                async move {
                    (
                        missing_target.dupe(),
                        setup.await.with_buck_error_context(|| {
                            format!(
                                "Error setting up local resource declared in `{missing_target}`"
                            )
                        }),
                    )
                }
            });
        for (target, result) in
            buck2_util::future::join_all(resource_futs.collect::<Vec<_>>()).await
        {
            lock.insert(target, result);
        }

        let result: buck2_error::Result<Vec<_>> = required_targets
            .iter()
            .map(|t| lock.get(t).unwrap().clone())
            .collect();
        Ok(result?)
    }

    async fn prepare_local_resource(
        dice: &mut DiceComputations<'_>,
        provider: (&ConfiguredTargetLabel, OwnedLocalResourceInfo),
        fs: &ArtifactFs,
        executor_fs: &ExecutorFs<'_>,
        default_timeout: Duration,
    ) -> buck2_error::Result<PreparedLocalResourceSetupContext> {
        let digest_config = dice.global_data().get_digest_config();

        let (target, provider) = provider;
        // The `'v`-branded view of the provider must not be held across an await (only the
        // `OwnedFrozen` may be), so this is scoped and re-derived below.
        let visited_inputs = {
            let info = provider.as_ref().value().as_ref();
            let mut artifact_visitor = SimpleCommandLineArtifactVisitor::new();
            info.setup_command_line()
                .visit_artifacts(&mut artifact_visitor)?;
            artifact_visitor.inputs
        };

        let inputs = dice
            .try_compute_join(visited_inputs, async |dice, group| {
                dice.ensure_artifact_group(&group).await
            })
            .await?;

        let info = provider.as_ref().value().as_ref();
        let artifact_path_mapping: BuckMutMap<_, _> = inputs
            .iter()
            .flat_map(|v| v.iter())
            .map(|(a, v)| (a, v.content_based_path_hash()))
            .collect();
        let mut cmd: Vec<String> = vec![];
        info.setup_command_line()
            .add_to_command_line(&mut CommandLineBuilder::new(
                &mut cmd,
                &artifact_path_mapping,
                executor_fs,
            ))?;

        let inputs = inputs
            .into_iter()
            .map(|group_values| CommandExecutionInput::Artifact(Box::new(group_values)))
            .collect();
        let paths = CommandExecutionPaths::new(
            inputs,
            buck_mut_insertion_set![],
            fs,
            digest_config,
            dice.per_transaction_data()
                .get_run_action_knobs()
                .action_paths_interner
                .as_ref(),
        )?;
        let mut execution_request =
            CommandExecutionRequest::new(vec![], cmd, paths, Default::default());
        execution_request =
            execution_request.with_timeout(info.setup_timeout().unwrap_or(default_timeout));
        execution_request = execution_request.with_skip_resource_control();
        Ok(PreparedLocalResourceSetupContext {
            target: target.dupe(),
            execution_request,
            env_var_mapping: info.env_var_mapping(),
        })
    }

    async fn start_local_resource(
        events: EventDispatcher,
        liveliness_observer: Arc<dyn LivelinessObserver>,
        digest_config: DigestConfig,
        executor: CommandExecutor,
        context: PreparedLocalResourceSetupContext,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LocalResourceState> {
        let manager = CommandExecutionManager::new(
            Box::new(MutexClaimManager::new()),
            events.dupe(),
            liveliness_observer,
            WaitingData::new(),
        );

        let local_resource_target = LocalResourceTarget {
            target: &context.target,
        };
        let prepared_action =
            executor.prepare_action(&context.execution_request, digest_config, false)?;
        let prepared_command = PreparedCommand {
            target: &local_resource_target as _,
            request: &context.execution_request,
            prepared_action: &prepared_action,
            digest_config,
        };
        let command = executor.exec_cmd(manager, &prepared_command, cancellation);

        let start = SetupLocalResourcesStart {
            target_label: Some(context.target.as_proto()),
        };
        let end = SetupLocalResourcesEnd {};
        let execution_result = events
            .span_async(start, async move { (command.await, end) })
            .await;

        let CommandExecutionResult {
            outputs: _,
            report:
                CommandExecutionReport {
                    std_streams,
                    exit_code,
                    status,
                    ..
                },
            ..
        } = execution_result;

        let std_streams = std_streams
            .into_bytes()
            .await
            .buck_error_context("Error accessing setup local resource output")?;

        match status {
            CommandExecutionStatus::Success { .. } => {}
            CommandExecutionStatus::Failure { .. }
            | CommandExecutionStatus::WorkerFailure { .. } => {
                return Err(buck2_error::buck2_error!(
                    ErrorTag::LocalResourceSetup,
                    "Local resource setup command failed with `{}` exit code, stdout:\n{}\nstderr:\n{}\n",
                    exit_code.unwrap_or(1),
                    String::from_utf8_lossy(&std_streams.stdout),
                    String::from_utf8_lossy(&std_streams.stderr),
                ));
            }
            CommandExecutionStatus::TimedOut { duration, .. } => {
                return Err(buck2_error::buck2_error!(
                    ErrorTag::LocalResourceSetup,
                    "Local resource setup command timed out after `{}s`, stdout:\n{}\nstderr:\n{}\n",
                    duration.as_secs(),
                    String::from_utf8_lossy(&std_streams.stdout),
                    String::from_utf8_lossy(&std_streams.stderr),
                ));
            }
            CommandExecutionStatus::Error { error, .. } => {
                return Err(error);
            }
            CommandExecutionStatus::Cancelled { .. } => {
                return Err(buck2_error::buck2_error!(
                    ErrorTag::LocalResourceSetup,
                    "Local resource setup command cancelled"
                ));
            }
        };

        let string_content = String::from_utf8_lossy(&std_streams.stdout);
        let data: LocalResourcesSetupResult = serde_json::from_str(&string_content)
            // .buck_error_context("Error parsing local resource setup command output")
            .map_err(|e| from_any_with_tag(e, ErrorTag::LocalResourceSetup))?;
        let state = data.into_state(context.target.dupe(), &context.env_var_mapping)?;

        Ok(state)
    }
}

impl Drop for BuckTestOrchestrator<'_> {
    fn drop(&mut self) {
        // If we didn't close the sender yet, then notify the receiver that our stream is
        // incomplete.
        let _ignored = self
            .results_channel
            .unbounded_send(Err(buck2_error::internal_error!(
                "BuckTestOrchestrator exited before end-of-tests was received",
            )));
    }
}

/// A command argument on its way to an executor, once argsfiles have been written.
enum ExpandableArg {
    Value(ArgValue),
    /// An argsfile the runner supplied, already written and part of the action's inputs.
    Argsfile(ProjectRelativePathBuf),
}

/// Renders an argsfile argument as `@<path>`.
fn push_argsfile_arg(
    fmt: &mut CommandLineBuilder<'_, '_>,
    path: ProjectRelativePathBuf,
) -> buck2_error::Result<()> {
    fmt.push_scope_format("@{}");
    fmt.push_project_path(path)?;
    fmt.pop_scope();
    Ok(())
}

struct Execute2RequestExpander<'a> {
    test_info: &'a OwnedTestInfo,
    stage: &'a TestStage,
    output_root: &'a ForwardRelativePath,
    declared_outputs: &'a mut BuckMutInsertionMap<BuckOutTestPath, OutputCreationBehavior>,
    fs: &'a ExecutorFs<'a>,
    cmd: Vec<ExpandableArg>,
    env: Cow<'a, BuckFixedSortedMap<String, ArgValue>>,
    digest_config: DigestConfig,
}

fn make_visit_arg_artifacts<'v>(
    cli_args_for_interpolation: Vec<&'v dyn CommandLineArgLike<'v>>,
    env_for_interpolation: BuckMutMap<&'v str, &'v dyn CommandLineArgLike<'v>>,
) -> impl for<'a> Fn(&'a mut dyn CommandLineArtifactVisitor<'v>, &'a ArgValue) -> buck2_error::Result<()>
{
    move |artifact_visitor: &mut dyn CommandLineArtifactVisitor<'_>, value: &ArgValue| {
        match &value.content {
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::ArgHandle(h)) => {
                let arg = cli_args_for_interpolation
                    .get(h.0)
                    .with_internal_error(|| format!("Invalid ArgHandle: {h:?}"))?;
                arg.visit_artifacts(artifact_visitor)?;
            }
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::EnvHandle(h)) => {
                let arg = env_for_interpolation
                    .get(h.0.as_str())
                    .with_internal_error(|| format!("Invalid EnvHandle: {h:?}"))?;
                arg.visit_artifacts(artifact_visitor)?;
            }
            ArgValueContent::DeclaredOutput(_)
            | ArgValueContent::ExternalRunnerSpecValue(_)
            | ArgValueContent::ArgsfileContents(_) => {}
        };

        buck2_error::Ok(())
    }
}

impl<'a> Execute2RequestExpander<'a> {
    fn get_inputs(&self) -> buck2_error::Result<BuckMutInsertionSet<ArtifactGroup>> {
        let Execute2RequestExpander {
            test_info,
            stage,
            cmd,
            env,
            ..
        } = self;
        let cli_args_for_interpolation = test_info.cli_args_for_stage(stage);
        let env_for_interpolation = test_info.env_args();

        let visit_arg_artifacts =
            make_visit_arg_artifacts(cli_args_for_interpolation, env_for_interpolation);

        let mut artifact_visitor = SimpleCommandLineArtifactVisitor::new();
        for var in cmd.iter() {
            if let ExpandableArg::Value(var) = var {
                visit_arg_artifacts(&mut artifact_visitor, var)?;
            }
        }

        for (_, var) in env.iter() {
            visit_arg_artifacts(&mut artifact_visitor, var)?;
        }
        let worker_exe = test_info.worker().map(|worker| worker.exe_command_line());
        if let Some(worker_exe) = worker_exe {
            worker_exe.visit_artifacts(&mut artifact_visitor)?;
        }

        Ok(artifact_visitor.inputs)
    }

    fn expand_arg_value<'v>(
        fmt: &mut CommandLineBuilder<'v, '_>,
        declared_outputs: &mut BuckMutInsertionMap<BuckOutTestPath, OutputCreationBehavior>,
        value: &'v ArgValue,
        cli_args_for_interpolation: &[&dyn CommandLineArgLike<'v>],
        env_for_interpolation: &BuckMutMap<&str, &dyn CommandLineArgLike<'v>>,
        output_root: &ForwardRelativePath,
        fs: &ExecutorFs<'_>,
    ) -> buck2_error::Result<()> {
        let ArgValue { content, format } = value;

        if let Some(format) = format {
            fmt.push_scope_format(format);
        }

        match content {
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::Verbatim(v)) => {
                v.as_str().add_to_command_line(fmt)?;
            }
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::ArgHandle(h)) => {
                let arg = cli_args_for_interpolation
                    .get(h.0)
                    .with_internal_error(|| format!("Invalid ArgHandle: {h:?}"))?;
                arg.add_to_command_line(fmt)?;
            }
            ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::EnvHandle(h)) => {
                let arg = env_for_interpolation
                    .get(h.0.as_str())
                    .with_internal_error(|| format!("Invalid EnvHandle: {h:?}"))?;
                arg.add_to_command_line(fmt)?;
            }
            ArgValueContent::ArgsfileContents(_) => {
                return Err(internal_error!(
                    "Argsfile contents are only valid as command arguments"
                ));
            }
            ArgValueContent::DeclaredOutput(output) => {
                let test_path = BuckOutTestPath::new(output_root.to_owned(), output.name.clone());
                let path = fs.fs().buck_out_path_resolver().resolve_test(&test_path);
                fmt.push_project_path(path)?;
                declared_outputs.insert(test_path, OutputCreationBehavior::Parent);
            }
        };

        if format.is_some() {
            fmt.pop_scope();
        }

        buck2_error::Ok(())
    }

    /// Expand a command and env.
    fn expand(
        self,
        ensured_inputs: &Vec<(ArtifactGroup, ArtifactGroupValues)>,
        absolute: bool,
    ) -> buck2_error::Result<(
        Vec<String>,
        BuckFixedSortedMap<String, String>,
        Option<WorkerSpec>,
    )> {
        let Execute2RequestExpander {
            test_info,
            stage,
            output_root,
            declared_outputs,
            fs,
            cmd,
            env,
            digest_config,
        } = self;
        let cli_args_for_interpolation = test_info.cli_args_for_stage(stage);
        let env_for_interpolation = test_info.env_args();

        let artifact_path_mapping = ArtifactPathMapperImpl::from(ensured_inputs);

        let mut expanded_cmd = Vec::<String>::new();
        let mut cmd_fmt = CommandLineBuilder::new_with_options(
            &mut expanded_cmd,
            &artifact_path_mapping,
            self.fs,
            absolute,
            None,
        );
        for var in cmd.iter() {
            match var {
                ExpandableArg::Argsfile(path) => push_argsfile_arg(&mut cmd_fmt, path.clone())?,
                ExpandableArg::Value(var) => Self::expand_arg_value(
                    &mut cmd_fmt,
                    declared_outputs,
                    var,
                    &cli_args_for_interpolation,
                    &env_for_interpolation,
                    output_root,
                    fs,
                )?,
            }
        }

        let expanded_env = env
            .as_ref()
            .into_iter()
            .map(|(k, v)| {
                let mut curr_env = SingletonCommandLineSink::new();
                let mut fmt = CommandLineBuilder::new_with_options(
                    &mut curr_env,
                    &artifact_path_mapping,
                    self.fs,
                    absolute,
                    None,
                );
                fmt.push_scope_delimiter(" ");
                Self::expand_arg_value(
                    &mut fmt,
                    declared_outputs,
                    &v,
                    &cli_args_for_interpolation,
                    &env_for_interpolation,
                    output_root,
                    fs,
                )?;
                fmt.pop_scope();
                buck2_error::Ok((k.to_owned(), curr_env.finalize()?))
            })
            .collect::<Result<BuckFixedSortedMap<_, _>, _>>()?;

        let expanded_worker = match test_info.worker() {
            Some(worker) => {
                let mut worker_rendered = Vec::<String>::new();
                let worker_exe = worker.exe_command_line();
                let mut fmt =
                    CommandLineBuilder::new(&mut worker_rendered, &artifact_path_mapping, self.fs);
                worker_exe.add_to_command_line(&mut fmt)?;
                let worker_env: buck2_error::Result<BuckFixedSortedMap<_, _>> = worker
                    .env()
                    .into_iter()
                    .map(|(k, v)| {
                        let mut env = SingletonCommandLineSink::new();
                        let mut fmt =
                            CommandLineBuilder::new(&mut env, &artifact_path_mapping, self.fs);
                        fmt.push_scope_delimiter(" ");
                        v.add_to_command_line(&mut fmt)?;
                        fmt.pop_scope();
                        Ok((k.to_owned(), env.finalize()?))
                    })
                    .collect();

                Some(WorkerSpec {
                    exe: worker_rendered,
                    id: WorkerId(worker.id),
                    env: worker_env?,
                    concurrency: worker.concurrency(),
                    streaming: worker.streaming(),
                    remote_key: None,
                    // TODO(ianc): Support input_paths on test workers
                    input_paths: CommandExecutionPaths::new(
                        vec![],
                        buck_mut_insertion_set![],
                        fs.fs(),
                        digest_config,
                        None,
                    )?,
                })
            }
            _ => None,
        };

        Ok((expanded_cmd, expanded_env, expanded_worker))
    }
}

async fn resolve_output_root(
    dice: &mut DiceComputations<'_>,
    test_target: &ConfiguredProvidersLabel,
    stage: &TestStage,
) -> Result<ForwardRelativePathBuf, buck2_error::Error> {
    let resolver = dice.get_buck_out_path().await?;

    let output_root = match stage {
        TestStage::Listing { .. } => resolver
            .resolve_test_discovery(test_target)?
            .into_forward_relative_path_buf(),
        TestStage::Testing {
            testcases,
            variant,
            repeat_count,
            ..
        } => {
            let mut hasher = DefaultHasher::new();
            variant.hash(&mut hasher);
            repeat_count.hash(&mut hasher);
            testcases.hash(&mut hasher);
            let extra_info_hashed = format!("{:016x}", hasher.finish());

            resolver
                .resolve_test_execution(
                    test_target,
                    ForwardRelativePath::unchecked_new(&extra_info_hashed),
                )?
                .into_forward_relative_path_buf()
        }
    };
    Ok(output_root)
}

struct ExpandedTestExecutable {
    cwd: ProjectRelativePathBuf,
    cmd: Vec<String>,
    env: BuckFixedSortedMap<String, String>,
    ensured_inputs: Vec<(ArtifactGroup, ArtifactGroupValues)>,
    supports_re: bool,
    declared_outputs: BuckMutInsertionMap<BuckOutTestPath, OutputCreationBehavior>,
    /// Argsfiles the runner supplied, already written; these become inputs of the action.
    argsfile_inputs: Vec<CommandExecutionInput>,
    worker: Option<WorkerSpec>,
}

fn create_prepare_for_local_execution_result(
    fs: &ArtifactFs,
    request: CommandExecutionRequest,
    local_resource_setup_commands: Vec<PreparedLocalResourceSetupContext>,
) -> PrepareForLocalExecutionResult {
    let relative_cwd = request.working_directory();
    let cwd = fs.fs().resolve(relative_cwd);
    let cmd = request.all_args_vec();

    let mut env = LossyEnvironment::new();
    apply_local_execution_environment(
        &mut env,
        &cwd,
        request.env(),
        request.local_environment_inheritance(),
    );

    let local_resource_setup_commands = local_resource_setup_commands
        .into_iter()
        .map(|r| local_resource_setup_command_prepared_for_local_execution(fs, r))
        .collect::<Vec<_>>();

    PrepareForLocalExecutionResult {
        command: LocalExecutionCommand {
            cmd,
            env: env.into_inner(),
            cwd,
        },
        local_resource_setup_commands,
    }
}

fn local_resource_setup_command_prepared_for_local_execution(
    fs: &ArtifactFs,
    resource_setup_command: PreparedLocalResourceSetupContext,
) -> LocalExecutionCommand {
    let relative_cwd = resource_setup_command.execution_request.working_directory();
    let cwd = fs.fs().resolve(relative_cwd);
    let cmd = resource_setup_command.execution_request.all_args_vec();

    let mut env = LossyEnvironment::new();
    apply_local_execution_environment(
        &mut env,
        &cwd,
        resource_setup_command.execution_request.env(),
        resource_setup_command
            .execution_request
            .local_environment_inheritance(),
    );

    LocalExecutionCommand {
        cmd,
        env: env.into_inner(),
        cwd,
    }
}

struct LossyEnvironment {
    inner: BuckMutInsertionMap<String, String>,
}

impl LossyEnvironment {
    fn new() -> Self {
        Self {
            inner: BuckMutInsertionMap::new(),
        }
    }

    fn into_inner(self) -> BuckFixedSortedMap<String, String> {
        self.inner.into()
    }
}

impl EnvironmentBuilder for LossyEnvironment {
    fn clear(&mut self) {
        self.inner.clear();
    }

    fn set<K, V>(&mut self, key: K, val: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.insert(
            key.as_ref().to_string_lossy().into_owned(),
            val.as_ref().to_string_lossy().into_owned(),
        );
    }

    fn remove<K>(&mut self, key: K)
    where
        K: AsRef<OsStr>,
    {
        self.inner.shift_remove(&*key.as_ref().to_string_lossy());
    }
}

#[derive(Debug)]
struct TestTarget<'a> {
    target: &'a ConfiguredTargetLabel,
    action_key_suffix: String,
}

impl CommandExecutionTarget for TestTarget<'_> {
    fn re_action_key(&self) -> String {
        format!("{} test {}", self.target, self.action_key_suffix)
    }

    fn re_affinity_key(&self) -> String {
        self.target.to_string()
    }

    fn as_proto_action_key(&self) -> buck2_data::ActionKey {
        buck2_data::ActionKey {
            id: Default::default(),
            owner: Some(buck2_data::action_key::Owner::TestTargetLabel(
                self.target.as_proto(),
            )),
            key: Default::default(),
        }
    }

    fn as_proto_action_name(&self) -> buck2_data::ActionName {
        buck2_data::ActionName {
            category: "test".to_owned(),
            identifier: "".to_owned(),
        }
    }
}

fn create_action_key_suffix(stage: &TestStage) -> String {
    let mut action_key_suffix = match &stage {
        TestStage::Listing { .. } => "listing".to_owned(),
        TestStage::Testing {
            testcases, variant, ..
        } => {
            if let Some(variant) = variant {
                format!("{} {}", variant, testcases.join(" "),)
            } else {
                testcases.join(" ")
            }
        }
    };
    if action_key_suffix.len() > MAX_SUFFIX_LEN {
        let truncated = "(truncated)";
        let max_len = MAX_SUFFIX_LEN - truncated.len();
        let truncate_at = action_key_suffix.floor_char_boundary(max_len);
        action_key_suffix.truncate(truncate_at);
        action_key_suffix += truncated;
    }
    action_key_suffix
}

#[derive(Debug)]
struct LocalResourceTarget<'a> {
    target: &'a ConfiguredTargetLabel,
}

impl CommandExecutionTarget for LocalResourceTarget<'_> {
    fn re_action_key(&self) -> String {
        String::new()
    }

    fn re_affinity_key(&self) -> String {
        String::new()
    }

    fn as_proto_action_key(&self) -> buck2_data::ActionKey {
        buck2_data::ActionKey {
            id: Default::default(),
            owner: Some(buck2_data::action_key::Owner::LocalResourceSetup(
                self.target.as_proto(),
            )),
            key: Default::default(),
        }
    }

    fn as_proto_action_name(&self) -> buck2_data::ActionName {
        buck2_data::ActionName {
            category: "setup_local_resource".to_owned(),
            identifier: "".to_owned(),
        }
    }
}

struct TestExecutor {
    test_executor: CommandExecutor,
    executor_config: CommandExecutorConfig,
}

impl TestExecutor {
    pub fn re_cache_enabled(&self) -> bool {
        self.executor_config.re_cache_enabled()
    }

    pub fn executor(&self) -> &CommandExecutor {
        &self.test_executor
    }

    pub fn re_dynamic_image(&self) -> Option<RemoteExecutorCustomImage> {
        if let Executor::RemoteEnabled(options) = &self.executor_config.executor {
            options.custom_image.clone().map(|image| *image)
        } else {
            None
        }
    }

    pub fn meta_internal_extra_params(&self) -> Arc<MetaInternalExtraParams> {
        if let Executor::RemoteEnabled(options) = &self.executor_config.executor {
            options.meta_internal_extra_params.clone()
        } else {
            MetaInternalExtraParams::default_arc()
        }
    }
}

/// The exit code reported for a failed execution. Runners read exit code 0 as a pass, and an
/// executor can fail with exit code 0 (a worker that exits 0 before connecting), so a failure
/// always reports a non-zero code.
fn failure_exit_code(exit_code: Option<i32>) -> i32 {
    match exit_code {
        Some(0) | None => 1,
        Some(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use buck2_build_api::context::SetBuildContextData;
    use buck2_common::dice::cells::SetCellResolver;
    use buck2_common::dice::data::testing::SetTestingIoProvider;
    use buck2_common::file_ops::baseline::FileSystemBaseline;
    use buck2_common::file_ops::baseline::SetFileSystemBaseline;
    use buck2_common::liveliness_observer::NoopLivelinessObserver;
    use buck2_core::cells::CellResolver;
    use buck2_core::cells::name::CellName;
    use buck2_core::configuration::data::ConfigurationData;
    use buck2_core::fs::buck_out_path::BuckOutPathResolver;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_execute::digest_config::SetDigestConfig;
    use buck2_execute::execute::manager::CommandExecutionManagerExt;
    use buck2_execute::execute::output::CommandStdStreams;
    use buck2_execute::execute::prepared::PreparedCommandExecutor;
    use buck2_execute::re::manager::UnconfiguredRemoteExecutionClient;
    use buck2_test_api::data::TestStage;
    use buck2_test_api::data::TestStatus;
    use buck2_util::time_span::TimeSpan;
    use dice::UserComputationData;
    use dice::testing::DiceBuilder;
    use futures::channel::mpsc;
    use futures::channel::mpsc::UnboundedReceiver;
    use futures::future;
    use futures::stream::TryStreamExt;

    use super::*;

    async fn make() -> buck2_error::Result<(
        BuckTestOrchestrator<'static>,
        UnboundedReceiver<buck2_error::Result<ExecutorMessage>>,
    )> {
        let fs = ProjectRootTemp::new().unwrap();

        let cell_resolver = CellResolver::testing_with_name_and_path(
            CellName::testing_new("cell"),
            CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".to_owned())),
        );
        let buckout_path = ProjectRelativePathBuf::unchecked_new("buck_out/v2".into());
        let mut dice = DiceBuilder::new()
            .set_data(|d| d.set_testing_io_provider(&fs))
            .build(UserComputationData::new())
            .await
            .unwrap();
        dice.set_buck_out_path(Some(buckout_path))?;
        dice.set_cell_resolver(cell_resolver)?;
        dice.set_file_system_baseline(FileSystemBaseline::unique())?;

        let dice = dice.commit().await;

        let (sender, receiver) = mpsc::unbounded();

        let re_client = Arc::new(remote_storage::ReClientWithCache::new(
            UnconfiguredRemoteExecutionClient::testing_new_dummy(),
        ));

        Ok((
            BuckTestOrchestrator::from_parts(
                dice,
                Arc::new(TestSession::new(Default::default())),
                NoopLivelinessObserver::create(),
                sender,
                EventDispatcher::null(),
                CancellationContext::testing(),
                re_client,
                InternalRunnerConfig::parse(None),
            ),
            receiver,
        ))
    }

    /// An executor that reports a failure with exit code 0, as a test worker that exits 0 before
    /// connecting does (`WorkerInitError::EarlyExit { exit_code: Some(0) }`).
    struct FailureWithExitCodeZero;

    #[async_trait]
    impl PreparedCommandExecutor for FailureWithExitCodeZero {
        async fn exec_cmd(
            &self,
            command: &PreparedCommand<'_, '_>,
            manager: CommandExecutionManager,
            _cancellations: &CancellationContext,
        ) -> CommandExecutionResult {
            let kind = CommandExecutionKind::Local {
                digest: command.prepared_action.digest(),
                command: vec![],
                env: BuckFixedSortedMap::new(),
            };
            manager.claim().await.failure(
                kind,
                Default::default(),
                CommandStdStreams::Local {
                    stdout: Vec::new(),
                    stderr: b"worker exited before connecting".to_vec(),
                },
                Some(0),
                CommandExecutionMetadata::empty(TimeSpan::empty_now()),
                None,
                false,
            )
        }

        fn is_local_execution_possible(&self, _executor_preference: ExecutorPreference) -> bool {
            true
        }
    }

    /// A failed execution is never reported with exit code 0, which the runners take as a pass
    /// and `TestExecutionKey::validity` caches as a successful listing.
    #[tokio::test]
    async fn failure_with_exit_code_zero_is_reported_as_a_failure() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new().unwrap();
        let digest_config = DigestConfig::testing_default();
        let mut data = UserComputationData::new();
        data.data.set(EventDispatcher::null());
        let dice = DiceBuilder::new()
            .set_data(|d| {
                d.set_testing_io_provider(&temp);
                d.set_digest_config(digest_config);
            })
            .build(data)
            .await
            .unwrap()
            .commit()
            .await;

        let fs = ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".to_owned())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck_out/v2".into())),
            temp.path().dupe(),
        );
        let executor = CommandExecutor::new(
            Arc::new(FailureWithExitCodeZero),
            Arc::new(NoOpCommandOptionalExecutor {}),
            Arc::new(NoOpCommandOptionalExecutor {}),
            Arc::new(NoOpCacheUploader {}),
            None,
            fs.dupe(),
            CommandGenerationOptions {
                path_separator: PathSeparatorKind::system_default(),
                output_paths_behavior: Default::default(),
                use_bazel_protocol_remote_persistent_workers: false,
                network_access: None,
            },
            remote_execution::Platform { properties: vec![] },
        );
        let paths = CommandExecutionPaths::new(
            vec![],
            BuckMutInsertionSet::default(),
            &fs,
            digest_config,
            None,
        )?;
        let request = CommandExecutionRequest::new(
            vec![],
            vec!["false".to_owned()],
            paths,
            BuckFixedSortedMap::new(),
        );
        let label = ConfiguredProvidersLabel::new(
            ConfiguredTargetLabel::testing_parse("cell//pkg:foo", ConfigurationData::testing_new()),
            Default::default(),
        );
        let stage = TestStage::Testing {
            suite: "foo".to_owned(),
            testcases: vec![],
            variant: None,
            repeat_count: None,
        };

        let mut ctx = dice.ctx();
        let data = buck2_events::dispatch::with_dispatcher_async(
            EventDispatcher::null(),
            BuckTestOrchestrator::execute_request(
                &mut ctx,
                CancellationContext::testing(),
                &label,
                &stage,
                &executor,
                request,
                NoopLivelinessObserver::create(),
                false,
                false,
                None,
                None,
                WaitingData::new(),
            ),
        )
        .await
        .map_err(|_| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "execute_request returned an error or a cancellation"
            )
        })?;
        assert_eq!(ExecutionStatus::Finished { exitcode: 1 }, data.status);
        Ok(())
    }

    #[tokio::test]
    async fn orchestrator_results() -> buck2_error::Result<()> {
        let (orchestrator, channel) = make().await?;

        let target =
            ConfiguredTargetLabel::testing_parse("cell//pkg:foo", ConfigurationData::testing_new());

        let target = ConfiguredProvidersLabel::new(target, Default::default());
        let target = orchestrator.session.register(target);

        let jobs = async {
            orchestrator
                .report_test_result(TestResult {
                    target,
                    status: TestStatus::PASS,
                    msg: None,
                    name: "First - test".to_owned(),
                    duration: Some(Duration::from_micros(1)),
                    details: "1".to_owned(),
                    max_memory_used_bytes: None,
                    origin: Default::default(),
                })
                .await?;

            orchestrator
                .report_test_result(TestResult {
                    target,
                    status: TestStatus::FAIL,
                    msg: None,
                    name: "Second - test".to_owned(),
                    duration: Some(Duration::from_micros(2)),
                    details: "2".to_owned(),
                    max_memory_used_bytes: None,
                    origin: Default::default(),
                })
                .await?;

            orchestrator.end_of_test_results(0).await?;

            buck2_error::Ok(())
        };

        let ((), results) = future::try_join(jobs, channel.try_collect::<Vec<_>>()).await?;

        assert_eq!(
            results,
            vec![
                ExecutorMessage::TestResult(TestResult {
                    target,

                    status: TestStatus::PASS,
                    msg: None,
                    name: "First - test".to_owned(),
                    duration: Some(Duration::from_micros(1)),
                    details: "1".to_owned(),
                    max_memory_used_bytes: None,
                    origin: Default::default(),
                }),
                ExecutorMessage::TestResult(TestResult {
                    target,

                    status: TestStatus::FAIL,
                    msg: None,
                    name: "Second - test".to_owned(),
                    duration: Some(Duration::from_micros(2)),
                    details: "2".to_owned(),
                    max_memory_used_bytes: None,
                    origin: Default::default(),
                }),
                ExecutorMessage::ExitCode(0),
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn orchestrator_attach_info_messages() -> buck2_error::Result<()> {
        let (orchestrator, channel) = make().await?;

        let jobs = async {
            orchestrator.attach_info_message("yolo".to_owned()).await?;

            orchestrator.end_of_test_results(0).await?;

            buck2_error::Ok(())
        };

        let ((), results) = future::try_join(jobs, channel.try_collect::<Vec<_>>()).await?;

        assert_eq!(
            results,
            vec![
                ExecutorMessage::InfoMessage("yolo".to_owned(),),
                ExecutorMessage::ExitCode(0),
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_orchestrator_channel_drop() -> buck2_error::Result<()> {
        let (orchestrator, channel) = make().await?;
        drop(orchestrator);

        let res = channel.try_collect::<Vec<_>>().await;
        assert!(res.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_orchestrator_closes_channel() -> buck2_error::Result<()> {
        let (orchestrator, channel) = make().await?;
        let sender = orchestrator.results_channel.clone();
        orchestrator.end_of_test_results(1).await?;

        assert!(sender.is_closed());
        drop(channel);

        Ok(())
    }

    /// EX-S1-01: a constrained case's launch check, through `execute_request` and the disk-state
    /// materializer with real disk IO. The check must read the executable value the action's
    /// input tree binds, whatever was or was not at its path before: a path-only
    /// `ensure_materialized` is deliberately a no-op in that materializer (`v2/core.rs`
    /// `materialize_many`), so it can neither put a cold executable in place nor replace a
    /// stale one.
    #[cfg(unix)]
    mod launch_check_with_disk_state_materializer {
        use std::path::PathBuf;
        use std::sync::Mutex;

        use buck2_common::file_ops::metadata::FileMetadata;
        use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
        use buck2_core::fs::project::ProjectRoot;
        use buck2_directory::directory::entry::DirectoryEntry;
        use buck2_execute::artifact_value::ArtifactValue;
        use buck2_execute::digest_config::SetDigestConfig;
        use buck2_execute::directory::ActionDirectoryMember;
        use buck2_execute::execute::blocking::testing::DummyBlockingExecutor;
        use buck2_execute::execute::environment_inheritance::EnvironmentInheritance;
        use buck2_execute::execute::output::CommandStdStreams;
        use buck2_execute::execute::prepared::PreparedCommandExecutor;
        use buck2_execute::materialize::materializer::SetMaterializer;
        use buck2_execute::materialize::produce::produce_locally;
        use buck2_execute::re::invocation_re_settings::InvocationReSettings;
        use buck2_execute::re::invocation_re_settings::SetInvocationReSettings;
        use buck2_execute::re::manager::ReConnectionManager;
        use buck2_execute_impl::materializers::deferred::AccessTimesUpdates;
        use buck2_execute_impl::materializers::deferred::DeferredMaterializerConfigs;
        use buck2_execute_impl::materializers::deferred::TtlRefreshConfiguration;
        use buck2_execute_impl::materializers::deferred::clean_stale::CleanStaleConfig;
        use buck2_execute_impl::materializers::v2::core::DiskStateMaterializer;
        use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
        use buck2_re_configuration::RemoteExecutionStaticMetadata;
        use buck2_util::time_span::TimeSpan;
        use dice::UserComputationData;
        use dice::testing::DiceBuilder;

        use super::*;
        use crate::local_result_cache::LocalCaseCandidate;

        /// The launched executable, an input of the action.
        const EXE: &str = "buck-out/v2/gen/t/launched";
        /// Where another action produced the same content earlier, which is where the
        /// materializer can copy it from.
        const PRODUCED: &str = "buck-out/v2/gen/producer/out";

        fn path(s: &str) -> ProjectRelativePathBuf {
            ProjectRelativePathBuf::unchecked_new(s.to_owned())
        }

        fn value(content: &[u8]) -> FileMetadata {
            FileMetadata {
                digest: TrackedFileDigest::from_content(
                    content,
                    DigestConfig::testing_default().cas_digest_config(),
                ),
                is_executable: true,
            }
        }

        /// The declared shell's store directory (`crate::testing`), whose `sh` is a bound
        /// interpreter. Never a host path.
        fn store_dir() -> String {
            crate::testing::declared_shell_dir()
        }

        /// The daemon's disk-state materializer over `project`, with real disk IO and no CAS.
        fn disk_state(project: &ProjectRoot, strict_leases: bool) -> Arc<dyn Materializer> {
            let buck_out = path("buck-out/v2");
            let re = Arc::new(ReConnectionManager::new(
                buck2_common::fbinit::get_or_init_fbcode_globals(),
                false,
                0,
                Arc::new(RemoteExecutionStaticMetadata::default()),
                None,
                project
                    .root()
                    .join(ForwardRelativePath::unchecked_new("buck-out/v2")),
                false,
            ));
            Arc::new(
                DiskStateMaterializer::new(
                    project.dupe(),
                    DigestConfig::testing_default(),
                    buck_out,
                    re,
                    Arc::new(DummyBlockingExecutor { fs: project.dupe() }),
                    DeferredMaterializerConfigs {
                        materialize_final_artifacts: true,
                        defer_write_actions: false,
                        ttl_refresh: TtlRefreshConfiguration {
                            frequency: Duration::default(),
                            min_ttl: Default::default(),
                            enabled: false,
                        },
                        update_access_times: AccessTimesUpdates::Disabled,
                        verbose_materializer_log: false,
                        clean_stale_config: CleanStaleConfig::default(),
                        readonly_outputs: false,
                        strict_leases,
                    },
                    None,
                    None,
                    EventDispatcher::null(),
                )
                .unwrap(),
            )
        }

        /// Another action's local run producing `content` at `PRODUCED`, which the materializer
        /// then tracks.
        async fn produce(materializer: &dyn Materializer, project: &ProjectRoot, content: &[u8]) {
            let disk = project.root().as_path().join(PRODUCED);
            let bytes = content.to_vec();
            produce_locally(
                materializer,
                &DummyBlockingExecutor { fs: project.dupe() },
                path(PRODUCED),
                ArtifactValue::file(value(content)),
                move || {
                    std::fs::create_dir_all(disk.parent().unwrap())?;
                    std::fs::write(&disk, &bytes)?;
                    std::fs::set_permissions(
                        &disk,
                        std::os::unix::fs::PermissionsExt::from_mode(0o755),
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        }

        /// What the executor was asked to run: the request's executor preference, which is
        /// `LocalRequired` only for a case that stayed constrained, and the bytes at `EXE` when
        /// it started.
        #[derive(Debug, Clone, PartialEq)]
        struct Started {
            preference: ExecutorPreference,
            exe: Option<Vec<u8>>,
        }

        struct Recording {
            exe: PathBuf,
            started: Arc<Mutex<Vec<Started>>>,
        }

        #[async_trait]
        impl PreparedCommandExecutor for Recording {
            async fn exec_cmd(
                &self,
                command: &PreparedCommand<'_, '_>,
                manager: CommandExecutionManager,
                _cancellations: &CancellationContext,
            ) -> CommandExecutionResult {
                self.started.lock().unwrap().push(Started {
                    preference: command.request.executor_preference(),
                    exe: std::fs::read(&self.exe).ok(),
                });
                manager.claim().await.success(
                    CommandExecutionKind::Local {
                        digest: command.prepared_action.digest(),
                        command: vec![],
                        env: BuckFixedSortedMap::new(),
                    },
                    Default::default(),
                    CommandStdStreams::Local {
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                    CommandExecutionMetadata::empty(TimeSpan::empty_now()),
                )
            }

            fn is_local_execution_possible(
                &self,
                _executor_preference: ExecutorPreference,
            ) -> bool {
                true
            }
        }

        /// The request a constrained local case runs: `EXE`, bound to `requested`, launched
        /// with a store `PATH`, and constrained as the orchestrator constrains it.
        fn constrained_case(
            fs: &ArtifactFs,
            requested: &[u8],
            cache_root: &str,
        ) -> (CommandExecutionRequest, ConstrainedCase) {
            let digest_config = DigestConfig::testing_default();
            let input = CommandExecutionInput::IncrementalRemoteOutput(
                path(EXE),
                DirectoryEntry::Leaf(ActionDirectoryMember::File(value(requested))),
            );
            let paths = CommandExecutionPaths::new(
                vec![input],
                Default::default(),
                fs,
                digest_config,
                None,
            )
            .unwrap();
            let env: BuckFixedSortedMap<String, String> =
                [("PATH".to_owned(), store_dir())].into_iter().collect();
            let request = CommandExecutionRequest::new(
                vec![],
                vec![EXE.to_owned(), "module::case".to_owned()],
                paths,
                env,
            )
            .with_local_environment_inheritance(EnvironmentInheritance::empty())
            .with_executor_preference(ExecutorPreference::LocalPreferred);
            let (request, case) = LocalCaseCandidate::testing_new(cache_root.to_owned()).constrain(
                request,
                RequestFacts {
                    project_fs: fs.fs(),
                    buck_out: vec!["buck-out".to_owned(), "v2".to_owned()],
                },
                None,
                |_| true,
            );
            (
                request,
                case.expect("the case is eligible before its launch check"),
            )
        }

        /// One constrained case through `execute_request`, with `on_disk` left at `EXE` by
        /// something the materializer does not track (an earlier daemon, an older build), or
        /// nothing there, and `requested` the value the input tree binds.
        async fn launch(on_disk: Option<&[u8]>, requested: &[u8]) -> Vec<Started> {
            let temp = ProjectRootTemp::new().unwrap();
            let project = temp.path();
            let digest_config = DigestConfig::testing_default();
            let materializer = disk_state(project, false);
            produce(&*materializer, project, requested).await;
            let exe = project.root().as_path().join(EXE);
            if let Some(stale) = on_disk {
                std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
                std::fs::write(&exe, stale).unwrap();
            }

            let mut data = UserComputationData::new();
            data.data.set(EventDispatcher::null());
            data.set_materializer(materializer.dupe());
            data.set_invocation_re_settings(InvocationReSettings {
                use_case: RemoteExecutorUseCase::buck2_default(),
                cas_configured: false,
                may_probe_cas: false,
            });
            let dice = DiceBuilder::new()
                .set_data(|d| {
                    d.set_testing_io_provider(&temp);
                    d.set_digest_config(digest_config);
                })
                .build(data)
                .await
                .unwrap()
                .commit()
                .await;
            let fs = ArtifactFs::new(
                CellResolver::testing_with_name_and_path(
                    CellName::testing_new("cell"),
                    CellRootPathBuf::new(path("cell")),
                ),
                BuckOutPathResolver::new(path("buck-out/v2")),
                project.dupe(),
            );
            let started = Arc::new(Mutex::new(Vec::new()));
            let executor = CommandExecutor::new(
                Arc::new(Recording {
                    exe: exe.clone(),
                    started: started.dupe(),
                }),
                Arc::new(NoOpCommandOptionalExecutor {}),
                Arc::new(NoOpCommandOptionalExecutor {}),
                Arc::new(NoOpCacheUploader {}),
                None,
                fs.dupe(),
                CommandGenerationOptions {
                    path_separator: PathSeparatorKind::system_default(),
                    output_paths_behavior: Default::default(),
                    use_bazel_protocol_remote_persistent_workers: false,
                    network_access: None,
                },
                remote_execution::Platform { properties: vec![] },
            );
            let cache_root = project.root().as_path().join("local-case-results");
            let (request, case) = constrained_case(&fs, requested, cache_root.to_str().unwrap());
            let label = ConfiguredProvidersLabel::new(
                ConfiguredTargetLabel::testing_parse(
                    "cell//pkg:foo",
                    ConfigurationData::testing_new(),
                ),
                Default::default(),
            );
            let stage = TestStage::Testing {
                suite: "foo".to_owned(),
                testcases: vec!["module::case".to_owned()],
                variant: None,
                repeat_count: None,
            };
            let mut ctx = dice.ctx();
            let result = buck2_events::dispatch::with_dispatcher_async(
                EventDispatcher::null(),
                BuckTestOrchestrator::execute_request(
                    &mut ctx,
                    CancellationContext::testing(),
                    &label,
                    &stage,
                    &executor,
                    request,
                    NoopLivelinessObserver::create(),
                    false,
                    false,
                    Some(case),
                    None,
                    WaitingData::new(),
                ),
            )
            .await;
            assert!(result.is_ok(), "execute_request failed");
            started.lock().unwrap().clone()
        }

        fn constrained(exe: &[u8]) -> Vec<Started> {
            vec![Started {
                preference: ExecutorPreference::LocalRequired,
                exe: Some(exe.to_vec()),
            }]
        }

        /// The legacy request: the case was given up, and runs as if reuse were never asked.
        fn legacy(exe: Option<&[u8]>) -> Vec<Started> {
            vec![Started {
                preference: ExecutorPreference::LocalPreferred,
                exe: exe.map(<[u8]>::to_vec),
            }]
        }

        /// A stale non-`#!` file at the path must not stand in for a requested script whose
        /// interpreter is a host tool: the requested script is what runs, so the case is
        /// refused, and the bytes the executor finds are the requested ones.
        #[tokio::test]
        async fn a_stale_binary_does_not_admit_a_requested_host_interpreter_script() {
            let requested: &[u8] = b"#!/bin/sh\necho requested\n";
            let started = launch(Some(b"\x7fELF stale binary"), requested).await;
            assert_eq!(started, legacy(Some(requested)));
        }

        /// A cold executable, absent from disk, is materialized for the check, and a valid
        /// case keeps its reuse.
        #[tokio::test]
        async fn a_cold_executable_is_materialized_and_the_case_stays_eligible() {
            let requested: &[u8] = b"\x7fELF requested binary";
            assert_eq!(launch(None, requested).await, constrained(requested));
        }

        /// A requested script with a bound store interpreter is admitted even when an
        /// unbound script was left at its path, since that is not what runs.
        #[tokio::test]
        async fn a_requested_store_interpreter_script_is_admitted_over_a_stale_host_script() {
            let requested = format!("#!{}/sh\necho requested\n", store_dir()).into_bytes();
            let started = launch(Some(b"#!/bin/sh\necho stale\n"), &requested).await;
            assert_eq!(started, constrained(&requested));
        }

        /// Under strict leases, which remove a tracked intermediate root the moment its last
        /// lease ends, the checked executable stays on disk, and no writer gets its path, for
        /// exactly as long as the check holds it; afterwards it is gone, so nothing could have
        /// read it outside the lease.
        #[tokio::test]
        async fn the_checked_value_is_held_for_the_check_under_strict_leases() {
            use buck2_execute::materialize::materializer::DeclareArtifactPayload;
            use buck2_execute::materialize::materializer::HeldContent;

            let temp = ProjectRootTemp::new().unwrap();
            let project = temp.path();
            let materializer = disk_state(project, true);
            let requested = format!("#!{}/sh\necho requested\n", store_dir()).into_bytes();
            // The producer keeps its own output, or strict cleanup would take the source away.
            let produced = match materializer
                .hold_or_take(path(PRODUCED), ArtifactValue::file(value(&requested)))
                .await
                .unwrap()
            {
                HeldContent::Found(_) => unreachable!("nothing was produced yet"),
                HeldContent::Missing(lease) => {
                    let disk = project.root().as_path().join(PRODUCED);
                    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
                    std::fs::write(&disk, &requested).unwrap();
                    materializer
                        .declare_existing(
                            &lease,
                            vec![DeclareArtifactPayload {
                                path: path(PRODUCED),
                                artifact: ArtifactValue::file(value(&requested)),
                            }],
                        )
                        .await
                        .unwrap();
                    lease.into_read_lease()
                }
            };
            let exe = project.root().as_path().join(EXE);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"\x7fELF stale binary").unwrap();
            let fs = ArtifactFs::new(
                CellResolver::testing_with_name_and_path(
                    CellName::testing_new("cell"),
                    CellRootPathBuf::new(path("cell")),
                ),
                BuckOutPathResolver::new(path("buck-out/v2")),
                project.dupe(),
            );
            let cache_root = project.root().as_path().join("local-case-results");
            let (_, case) = constrained_case(&fs, &requested, cache_root.to_str().unwrap());

            let held = case
                .launch()
                .clone()
                .materialize(&*materializer, RemoteExecutorUseCase::buck2_default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(std::fs::read(&exe).unwrap(), requested);
            held.verify().unwrap();
            // A writer at the path waits for the check's lease.
            let mut writer = Box::pin(materializer.prepare_outputs(vec![path(EXE)]));
            assert!(
                tokio::time::timeout(Duration::from_millis(300), &mut writer)
                    .await
                    .is_err(),
                "a writer took the checked path while the check held it"
            );
            assert_eq!(std::fs::read(&exe).unwrap(), requested);
            drop(held);
            tokio::time::timeout(Duration::from_secs(10), writer)
                .await
                .expect("the writer is granted once the check releases its lease")
                .unwrap()
                .abandon();

            // Held again, then released with nobody else holding it: strict cleanup removes it.
            let held = case
                .launch()
                .clone()
                .materialize(&*materializer, RemoteExecutorUseCase::buck2_default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(std::fs::read(&exe).unwrap(), requested);
            held.verify().unwrap();
            drop(held);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while exe.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "strict cleanup kept the released executable"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            drop(produced);
        }

        /// EX-S1-01 end to end, with nothing substituted for the final executor. A normal
        /// Artifact-backed test request goes through `execute_request`: the launch check
        /// materializes and reads the requested value, then the materializer replaces or removes
        /// the content at the executable's path, and then the real `LocalExecutor` runs the
        /// request over the same disk-state materializer and starts a real child.
        ///
        /// The check's lease ends before execution (`MaterializedLaunch`). What makes the started
        /// bytes the checked ones is the executor's own `materialize_inputs`, which requests the
        /// same Artifact value at the same path. Each launched script prints its own bytes, so
        /// the child's stdout is the content the kernel actually executed.
        mod real_local_executor {
            use buck2_artifact::actions::key::ActionIndex;
            use buck2_artifact::artifact::artifact_dyn::ArtifactDyn;
            use buck2_artifact::artifact::artifact_type::Artifact;
            use buck2_artifact::artifact::artifact_type::testing::BuildArtifactTestingExt;
            use buck2_artifact::artifact::build_artifact::BuildArtifact;
            use buck2_events::daemon_id::DaemonId;
            use buck2_execute::execute::result::CommandExecutionStatus;
            use buck2_execute::knobs::ExecutorGlobalKnobs;
            use buck2_execute_impl::executors::local::ForkserverAccess;
            use buck2_execute_impl::executors::local::LocalExecutor;
            use buck2_execute_impl::sqlite::incremental_state_db::IncrementalDbState;
            use host_sharing::HostSharingBroker;
            use host_sharing::HostSharingStrategy;

            use super::*;

            /// What happens through the materializer at the executable's path after the launch
            /// check has ended and before the real executor runs.
            #[derive(Debug, Clone, Copy, PartialEq)]
            enum Between {
                Nothing,
                /// Another action's local run produces different content at the path, which the
                /// materializer then tracks as that path's artifact.
                Replace,
                /// The path is taken for writing, its content deleted and left untracked.
                Remove,
            }

            /// Prints the script's own bytes using only shell builtins, so no tool beyond the
            /// interpreter is needed.
            const SELF_PRINT: &str =
                "while IFS= read -r line; do printf '%s\\n' \"$line\"; done < \"$0\"\n";

            /// The interpreters one run uses.
            struct Shells {
                /// The declared shell: a canonical Nix store path, which the launch check admits.
                store: PathBuf,
                /// A non-store symlink, inside the run's temporary project, to the same declared
                /// shell: it runs, but the launch check refuses it as not a Nix store path.
                alias: PathBuf,
            }

            impl Shells {
                fn new(project: &ProjectRoot) -> Self {
                    Self {
                        store: crate::testing::declared_shell(),
                        alias: crate::testing::non_store_shell(
                            &project.root().as_path().join("host-shell"),
                        ),
                    }
                }

                /// The `PATH` of the request: the declared shell's store directory.
                fn path_env(&self) -> String {
                    self.store.parent().unwrap().to_str().unwrap().to_owned()
                }

                /// A script with the declared store interpreter, which the launch check admits.
                fn store_script(&self) -> Vec<u8> {
                    format!("#!{}\n{SELF_PRINT}", self.store.display()).into_bytes()
                }

                /// A script with a non-store interpreter, which the launch check refuses and
                /// which still runs.
                fn host_script(&self) -> Vec<u8> {
                    format!("#!{}\n{SELF_PRINT}", self.alias.display()).into_bytes()
                }

                /// The replacement: a script the launch check would have refused, and that
                /// prints a marker rather than the requested bytes.
                fn replaced(&self) -> Vec<u8> {
                    format!("#!{}\necho REPLACED-BYTES-RAN\n", self.alias.display()).into_bytes()
                }
            }

            /// What one run did, as the real executor reports it.
            #[derive(Debug)]
            struct Run {
                /// The preference of the request handed to the executor: `LocalRequired` only
                /// for a case that stayed constrained after its launch check.
                preference: ExecutorPreference,
                /// The bytes at the executable's path just before the real executor took over,
                /// which show that the intervening step took effect.
                at_handoff: Option<Vec<u8>>,
                status: String,
                exit_code: Option<i32>,
                stdout: Vec<u8>,
                stderr: Vec<u8>,
                /// The bytes at the executable's path after the run.
                after: Option<Vec<u8>>,
            }

            /// Performs the intervening materializer step and then hands the command to the real
            /// `LocalExecutor`. It returns the real executor's result unchanged and does not
            /// finalize anything itself.
            struct ThenReal {
                between: Between,
                replaced: Vec<u8>,
                project: ProjectRoot,
                exe: ProjectRelativePathBuf,
                materializer: Arc<dyn Materializer>,
                real: LocalExecutor,
                runs: Arc<Mutex<Vec<Run>>>,
            }

            impl ThenReal {
                async fn intervene(&self) {
                    let disk = self.project.root().as_path().join(self.exe.as_str());
                    match self.between {
                        Between::Nothing => {}
                        Between::Replace => {
                            let made = produce_locally(
                                &*self.materializer,
                                &DummyBlockingExecutor {
                                    fs: self.project.dupe(),
                                },
                                self.exe.clone(),
                                ArtifactValue::file(value(&self.replaced)),
                                {
                                    let disk = disk.clone();
                                    let replaced = self.replaced.clone();
                                    move || {
                                        // A local run's output replaces the whole path.
                                        match std::fs::remove_file(&disk) {
                                            Ok(()) => {}
                                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                            Err(e) => return Err(e.into()),
                                        }
                                        std::fs::create_dir_all(disk.parent().unwrap())?;
                                        std::fs::write(&disk, &replaced)?;
                                        std::fs::set_permissions(
                                            &disk,
                                            std::os::unix::fs::PermissionsExt::from_mode(0o755),
                                        )?;
                                        Ok(())
                                    }
                                },
                            )
                            .await
                            .unwrap();
                            assert_eq!(
                                made,
                                buck2_execute::materialize::produce::Produced::Made,
                                "the replacement was not made through the materializer"
                            );
                        }
                        Between::Remove => {
                            let lease = self
                                .materializer
                                .prepare_outputs(vec![self.exe.clone()])
                                .await
                                .unwrap();
                            std::fs::remove_file(&disk)
                                .expect("the checked value is not on disk to be removed");
                            lease.leave_untracked(&self.exe).unwrap();
                        }
                    }
                }
            }

            #[async_trait]
            impl PreparedCommandExecutor for ThenReal {
                async fn exec_cmd(
                    &self,
                    command: &PreparedCommand<'_, '_>,
                    manager: CommandExecutionManager,
                    cancellations: &CancellationContext,
                ) -> CommandExecutionResult {
                    self.intervene().await;
                    let at_handoff =
                        std::fs::read(self.project.root().as_path().join(self.exe.as_str())).ok();
                    let result = self.real.exec_cmd(command, manager, cancellations).await;
                    let (stdout, stderr) = match &result.report.std_streams {
                        CommandStdStreams::Local { stdout, stderr } => {
                            (stdout.clone(), stderr.clone())
                        }
                        other => panic!("not a local run: {other:?}"),
                    };
                    self.runs.lock().unwrap().push(Run {
                        preference: command.request.executor_preference(),
                        at_handoff,
                        status: match &result.report.status {
                            CommandExecutionStatus::Success { .. } => "success".to_owned(),
                            other => format!("{other:?}"),
                        },
                        exit_code: result.report.exit_code,
                        stdout,
                        stderr,
                        after: None,
                    });
                    result
                }

                fn is_local_execution_possible(
                    &self,
                    executor_preference: ExecutorPreference,
                ) -> bool {
                    self.real.is_local_execution_possible(executor_preference)
                }
            }

            /// Pad the shorter script with a trailing comment line so that both have the same
            /// length, and a check that compares only lengths cannot tell them apart.
            fn same_length(mut a: Vec<u8>, mut b: Vec<u8>) -> (Vec<u8>, Vec<u8>) {
                fn pad(script: &mut Vec<u8>, to: usize) {
                    if script.len() < to {
                        script.extend(std::iter::repeat_n(b'#', to - script.len() - 1));
                        script.push(b'\n');
                    }
                }
                let len = a.len().max(b.len());
                pad(&mut a, len);
                pad(&mut b, len);
                assert_eq!(a.len(), b.len());
                (a, b)
            }

            /// What a run's scripts were.
            struct Scripts {
                /// Content another action left tracked at the executable's path before the
                /// check, or nothing there.
                tracked_before: Option<Vec<u8>>,
                /// The value the request's Artifact input binds.
                requested: Vec<u8>,
            }

            /// What one run did, with the scripts it used.
            struct Outcome {
                runs: Vec<Run>,
                requested: Vec<u8>,
                replaced: Vec<u8>,
            }

            /// One Artifact-backed constrained case run through `execute_request` and the real
            /// `LocalExecutor`.
            /// - `scripts`: the run's scripts, made from its interpreters.
            /// - `between`: the materializer step after the check.
            async fn run(scripts: impl FnOnce(&Shells) -> Scripts, between: Between) -> Outcome {
                let temp = ProjectRootTemp::new().unwrap();
                let project = temp.path();
                let shells = Shells::new(project);
                let Scripts {
                    tracked_before,
                    requested,
                } = scripts(&shells);
                let requested = &requested[..];
                let replaced = shells.replaced();
                let digest_config = DigestConfig::testing_default();
                let materializer = disk_state(project, false);
                // The requested content exists elsewhere, where the materializer can copy it
                // from, as an earlier action's output.
                produce(&*materializer, project, requested).await;

                let fs = ArtifactFs::new(
                    CellResolver::testing_with_name_and_path(
                        CellName::testing_new("cell"),
                        CellRootPathBuf::new(path("cell")),
                    ),
                    BuckOutPathResolver::new(path("buck-out/v2")),
                    project.dupe(),
                );
                let target = ConfiguredTargetLabel::testing_parse(
                    "cell//pkg:foo",
                    ConfigurationData::testing_new(),
                );
                let artifact: Artifact =
                    BuildArtifact::testing_new(target.dupe(), "launched", ActionIndex::new(0))
                        .into();
                let exe = artifact.resolve_configuration_hash_path(&fs).unwrap();
                let exe_disk = project.root().as_path().join(exe.as_str());

                if let Some(before) = tracked_before.as_deref() {
                    let bytes = before.to_vec();
                    let disk = exe_disk.clone();
                    produce_locally(
                        &*materializer,
                        &DummyBlockingExecutor { fs: project.dupe() },
                        exe.clone(),
                        ArtifactValue::file(value(before)),
                        move || {
                            std::fs::create_dir_all(disk.parent().unwrap())?;
                            std::fs::write(&disk, &bytes)?;
                            std::fs::set_permissions(
                                &disk,
                                std::os::unix::fs::PermissionsExt::from_mode(0o755),
                            )?;
                            Ok(())
                        },
                    )
                    .await
                    .unwrap();
                }

                let mut data = UserComputationData::new();
                data.data.set(EventDispatcher::null());
                data.set_materializer(materializer.dupe());
                data.set_invocation_re_settings(InvocationReSettings {
                    use_case: RemoteExecutorUseCase::buck2_default(),
                    cas_configured: false,
                    may_probe_cas: false,
                });
                let dice = DiceBuilder::new()
                    .set_data(|d| {
                        d.set_testing_io_provider(&temp);
                        d.set_digest_config(digest_config);
                    })
                    .build(data)
                    .await
                    .unwrap()
                    .commit()
                    .await;

                let real = LocalExecutor::new(
                    fs.dupe(),
                    materializer.dupe(),
                    Arc::new(IncrementalDbState::db_disabled()),
                    Arc::new(DummyBlockingExecutor { fs: project.dupe() }),
                    Arc::new(HostSharingBroker::new(
                        HostSharingStrategy::SmallerTasksFirst,
                        1,
                    )),
                    project.root().to_buf(),
                    ForkserverAccess::None,
                    ExecutorGlobalKnobs::default(),
                    None,
                    None,
                    DaemonId::new(),
                    RemoteExecutorUseCase::buck2_default(),
                );
                let runs = Arc::new(Mutex::new(Vec::new()));
                let executor = CommandExecutor::new(
                    Arc::new(ThenReal {
                        between,
                        replaced: replaced.clone(),
                        project: project.dupe(),
                        exe: exe.clone(),
                        materializer: materializer.dupe(),
                        real,
                        runs: runs.dupe(),
                    }),
                    Arc::new(NoOpCommandOptionalExecutor {}),
                    Arc::new(NoOpCommandOptionalExecutor {}),
                    Arc::new(NoOpCacheUploader {}),
                    None,
                    fs.dupe(),
                    CommandGenerationOptions {
                        path_separator: PathSeparatorKind::system_default(),
                        output_paths_behavior: Default::default(),
                        use_bazel_protocol_remote_persistent_workers: false,
                        network_access: None,
                    },
                    remote_execution::Platform { properties: vec![] },
                );

                // A normal Artifact-backed input, as the orchestrator builds a test's inputs.
                let input =
                    CommandExecutionInput::Artifact(Box::new(ArtifactGroupValues::from_artifact(
                        artifact,
                        ArtifactValue::file(value(requested)),
                    )));
                let paths = CommandExecutionPaths::new(
                    vec![input],
                    Default::default(),
                    &fs,
                    digest_config,
                    None,
                )
                .unwrap();
                let env: BuckFixedSortedMap<String, String> =
                    [("PATH".to_owned(), shells.path_env())]
                        .into_iter()
                        .collect();
                let request = CommandExecutionRequest::new(
                    vec![],
                    vec![exe.as_str().to_owned(), "module::case".to_owned()],
                    paths,
                    env,
                )
                .with_local_environment_inheritance(EnvironmentInheritance::empty())
                .with_executor_preference(ExecutorPreference::LocalPreferred);
                let cache_root = project.root().as_path().join("local-case-results");
                let (request, case) =
                    LocalCaseCandidate::testing_new(cache_root.to_str().unwrap().to_owned())
                        .constrain(
                            request,
                            RequestFacts {
                                project_fs: fs.fs(),
                                buck_out: vec!["buck-out".to_owned(), "v2".to_owned()],
                            },
                            None,
                            |_| true,
                        );
                let case = case.expect("the case is eligible before its launch check");

                let label = ConfiguredProvidersLabel::new(target, Default::default());
                let stage = TestStage::Testing {
                    suite: "foo".to_owned(),
                    testcases: vec!["module::case".to_owned()],
                    variant: None,
                    repeat_count: None,
                };
                let mut ctx = dice.ctx();
                let result = buck2_events::dispatch::with_dispatcher_async(
                    EventDispatcher::null(),
                    BuckTestOrchestrator::execute_request(
                        &mut ctx,
                        CancellationContext::testing(),
                        &label,
                        &stage,
                        &executor,
                        request,
                        NoopLivelinessObserver::create(),
                        false,
                        false,
                        Some(case),
                        None,
                        WaitingData::new(),
                    ),
                )
                .await;
                assert!(result.is_ok(), "execute_request failed");
                let mut runs = std::mem::take(&mut *runs.lock().unwrap());
                assert_eq!(runs.len(), 1, "exactly one real run: {runs:?}");
                runs[0].after = std::fs::read(&exe_disk).ok();
                Outcome {
                    runs,
                    requested: requested.to_vec(),
                    replaced,
                }
            }

            /// The child ran exactly the requested bytes and succeeded.
            fn assert_ran_requested(run: &Run, requested: &[u8]) {
                assert_eq!(
                    (run.status.as_str(), run.exit_code),
                    ("success", Some(0)),
                    "the real run did not succeed; stderr: {}",
                    String::from_utf8_lossy(&run.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&run.stdout),
                    String::from_utf8_lossy(requested),
                    "the executed bytes are not the requested value"
                );
                // After the run the path holds the requested value, not the replacement.
                assert_eq!(run.after.as_deref(), Some(requested));
            }

            /// Control: with nothing in between, the admitted case runs constrained and the
            /// child executes the requested bytes.
            #[tokio::test]
            async fn with_nothing_in_between_the_real_executor_runs_the_checked_value() {
                let Outcome {
                    runs, requested, ..
                } = run(
                    |shells| Scripts {
                        tracked_before: None,
                        requested: shells.store_script(),
                    },
                    Between::Nothing,
                )
                .await;
                assert_eq!(runs[0].preference, ExecutorPreference::LocalRequired);
                assert_eq!(runs[0].at_handoff.as_deref(), Some(&requested[..]));
                assert_ran_requested(&runs[0], &requested);
            }

            /// After the check admits the requested store-interpreter script, another action
            /// replaces the content at its path through the materializer with a script whose
            /// interpreter is not a store path. The real executor requests the Artifact value again, so the replacement is
            /// re-materialized away, and the child executes the checked bytes, never the
            /// replacement.
            #[tokio::test]
            async fn a_replacement_after_the_check_is_rematerialized_before_the_real_run() {
                let Outcome {
                    runs,
                    requested,
                    replaced,
                } = run(
                    |shells| Scripts {
                        tracked_before: None,
                        requested: shells.store_script(),
                    },
                    Between::Replace,
                )
                .await;
                assert_eq!(runs[0].preference, ExecutorPreference::LocalRequired);
                assert_eq!(
                    runs[0].at_handoff.as_deref(),
                    Some(&replaced[..]),
                    "the replacement did not take effect before the handoff"
                );
                assert_ran_requested(&runs[0], &requested);
            }

            /// After the check, the content at the path is deleted and left untracked. The real
            /// executor materializes the requested value again and runs it.
            #[tokio::test]
            async fn a_removal_after_the_check_is_rematerialized_before_the_real_run() {
                let Outcome {
                    runs, requested, ..
                } = run(
                    |shells| Scripts {
                        tracked_before: None,
                        requested: shells.store_script(),
                    },
                    Between::Remove,
                )
                .await;
                assert_eq!(runs[0].preference, ExecutorPreference::LocalRequired);
                assert_eq!(
                    runs[0].at_handoff, None,
                    "the removal did not take effect before the handoff"
                );
                assert_ran_requested(&runs[0], &requested);
            }

            /// Another action left a tracked store-interpreter script at the path, of the same
            /// length as the requested value, a script whose interpreter is not a store path. The
            /// check reads the requested value, not the tracked one, so the case is refused and runs as its legacy request. The real
            /// executor runs the requested bytes, and an admission based on bytes that do not
            /// run never happens. A materializer replacement before the real run changes neither
            /// result.
            #[tokio::test]
            async fn a_tracked_admissible_script_does_not_admit_the_requested_refused_one() {
                let Outcome {
                    runs, requested, ..
                } = run(
                    |shells| {
                        let (requested, tracked) =
                            same_length(shells.host_script(), shells.store_script());
                        Scripts {
                            tracked_before: Some(tracked),
                            requested,
                        }
                    },
                    Between::Replace,
                )
                .await;
                assert_eq!(
                    runs[0].preference,
                    ExecutorPreference::LocalPreferred,
                    "the case was admitted on bytes that are not the ones that run"
                );
                assert_ran_requested(&runs[0], &requested);
            }
        }
    }

    #[test]
    fn test_create_action_key_suffix_listing() {
        let stage = TestStage::Listing {
            suite: "test_suite".to_owned(),
            cacheable: true,
        };
        assert_eq!(create_action_key_suffix(&stage), "listing");
    }

    #[test]
    fn test_create_action_key_suffix_testing_no_variant() {
        let stage = TestStage::Testing {
            suite: "test_suite".to_owned(),
            testcases: vec!["test1".to_owned(), "test2".to_owned()],
            variant: None,
            repeat_count: None,
        };
        assert_eq!(create_action_key_suffix(&stage), "test1 test2");
    }

    #[test]
    fn test_create_action_key_suffix_testing_with_variant() {
        let stage = TestStage::Testing {
            suite: "test_suite".to_owned(),
            testcases: vec!["test1".to_owned(), "test2".to_owned()],
            variant: Some("variant1".to_owned()),
            repeat_count: None,
        };
        assert_eq!(create_action_key_suffix(&stage), "variant1 test1 test2");
    }

    #[test]
    fn test_create_action_key_suffix_truncation() {
        let long_testcase = "a".repeat(MAX_SUFFIX_LEN + 100);
        let stage = TestStage::Testing {
            suite: "test_suite".to_owned(),
            testcases: vec![long_testcase],
            variant: None,
            repeat_count: None,
        };
        let result = create_action_key_suffix(&stage);
        assert_eq!(result.len(), MAX_SUFFIX_LEN);
        assert!(result.ends_with("(truncated)"));
    }

    #[test]
    fn test_create_action_key_suffix_truncation_multibyte_utf8() {
        // 3-byte char (中 = 0xE4 0xB8 0xAD); byte 1013 lands mid-character.
        let long_testcase = "中".repeat(MAX_SUFFIX_LEN);
        let stage = TestStage::Testing {
            suite: "test_suite".to_owned(),
            testcases: vec![long_testcase],
            variant: None,
            repeat_count: None,
        };
        let result = create_action_key_suffix(&stage);
        assert!(result.len() <= MAX_SUFFIX_LEN);
        assert!(result.ends_with("(truncated)"));
    }

    mod local_admission {
        use buck2_core::execution_types::executor_config::CacheUploadBehavior;
        use buck2_core::execution_types::executor_config::HybridExecutionLevel;
        use buck2_core::execution_types::executor_config::RePlatformFields;
        use buck2_core::execution_types::executor_config::RemoteEnabledExecutor;
        use buck2_core::execution_types::executor_config::RemoteEnabledExecutorOptions;
        use buck2_core::execution_types::executor_config::RemoteExecutorOptions;
        use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;

        use super::*;

        type Orchestrator = BuckTestOrchestrator<'static>;

        const OFF: LocalTestAdmissions = LocalTestAdmissions {
            internal_cases: false,
            workspace_tests: false,
        };
        const INTERNAL_CASES: LocalTestAdmissions = LocalTestAdmissions {
            internal_cases: true,
            workspace_tests: false,
        };
        const WORKSPACE_TESTS: LocalTestAdmissions = LocalTestAdmissions {
            internal_cases: false,
            workspace_tests: true,
        };
        const BOTH: LocalTestAdmissions = LocalTestAdmissions {
            internal_cases: true,
            workspace_tests: true,
        };

        fn options() -> CommandGenerationOptions {
            CommandGenerationOptions {
                path_separator: PathSeparatorKind::Unix,
                output_paths_behavior: Default::default(),
                use_bazel_protocol_remote_persistent_workers: false,
                network_access: None,
            }
        }

        fn local() -> CommandExecutorConfig {
            CommandExecutorConfig {
                executor: Executor::Local(LocalExecutorOptions::default()),
                options: options(),
            }
        }

        fn none() -> CommandExecutorConfig {
            CommandExecutorConfig {
                executor: Executor::None,
                options: options(),
            }
        }

        fn hybrid() -> CommandExecutorConfig {
            CommandExecutorConfig {
                executor: Executor::RemoteEnabled(RemoteEnabledExecutorOptions {
                    executor: RemoteEnabledExecutor::Hybrid {
                        local: LocalExecutorOptions::default(),
                        remote: RemoteExecutorOptions::default(),
                        level: HybridExecutionLevel::Limited,
                    },
                    re_properties: RePlatformFields {
                        properties: Arc::new(Default::default()),
                    },
                    re_use_case: RemoteExecutorUseCase::buck2_default(),
                    re_action_key: None,
                    cache_upload_behavior: CacheUploadBehavior::Disabled,
                    remote_cache_enabled: false,
                    remote_dep_file_cache_enabled: false,
                    dependencies: vec![],
                    gang_workers: vec![],
                    custom_image: None,
                    meta_internal_extra_params: MetaInternalExtraParams::default_arc(),
                    priority: None,
                }),
                options: options(),
            }
        }

        fn listing() -> TestStage {
            TestStage::Listing {
                suite: "suite".to_owned(),
                cacheable: true,
            }
        }

        fn testing() -> TestStage {
            TestStage::Testing {
                suite: "suite".to_owned(),
                testcases: vec!["case".to_owned()],
                variant: None,
                repeat_count: None,
            }
        }

        /// The admission of an executor the provider declared (`CommandExecutorConfig(...)`).
        fn admission(
            kind: TestProviderKind,
            provider_executor: Option<&CommandExecutorConfig>,
            caching: bool,
            admissions: LocalTestAdmissions,
        ) -> LocalExecution {
            let declared = provider_executor
                .map(|config| StarlarkCommandExecutorConfig::declared(Arc::new(config.clone())));
            Orchestrator::local_execution_for_test(kind, declared.as_ref(), caching, admissions)
        }

        #[test]
        fn an_execution_group_platform_executor_is_never_admitted() {
            // The provider's `default_executor` was recorded from the local execution platform of
            // its `exec_group`: the same executor as an explicit local one, but chosen by
            // execution platform resolution, so `--remote-only` governs it.
            for executor in [local(), hybrid(), none()] {
                let recorded = StarlarkCommandExecutorConfig::from_execution_group_platform(
                    Arc::new(executor),
                );
                for kind in [TestProviderKind::External, TestProviderKind::Internal] {
                    for admissions in [OFF, INTERNAL_CASES, WORKSPACE_TESTS, BOTH] {
                        for caching in [false, true] {
                            assert_eq!(
                                Orchestrator::local_execution_for_test(
                                    kind,
                                    Some(&recorded),
                                    caching,
                                    admissions
                                ),
                                LocalExecution::ByStrategy,
                                "{kind:?} {admissions:?} {caching} {recorded:?}"
                            );
                        }
                    }
                }
            }
            // The positive control: the same local executor, declared, is admitted.
            assert_eq!(
                admission(
                    TestProviderKind::Internal,
                    Some(&local()),
                    false,
                    INTERNAL_CASES
                ),
                LocalExecution::AdmittedInternalTest
            );
            assert_eq!(
                admission(
                    TestProviderKind::External,
                    Some(&local()),
                    false,
                    WORKSPACE_TESTS
                ),
                LocalExecution::AdmittedLocalWorkspaceTest
            );
        }

        #[test]
        fn an_external_test_with_an_explicit_uncached_local_executor_is_a_local_workspace_test() {
            // The resolved provider executor is the same whether it came from the provider's
            // `default_executor` or a named override; both reach here as `Some`.
            for admissions in [WORKSPACE_TESTS, BOTH] {
                assert_eq!(
                    admission(
                        TestProviderKind::External,
                        Some(&local()),
                        false,
                        admissions
                    ),
                    LocalExecution::AdmittedLocalWorkspaceTest,
                    "{admissions:?}"
                );
            }
        }

        #[test]
        fn a_platform_fallback_executor_is_never_admitted() {
            // `None` is the execution platform's fallback executor: the provider selected none.
            for kind in [TestProviderKind::External, TestProviderKind::Internal] {
                for admissions in [OFF, INTERNAL_CASES, WORKSPACE_TESTS, BOTH] {
                    for caching in [false, true] {
                        assert_eq!(
                            admission(kind, None, caching, admissions),
                            LocalExecution::ByStrategy,
                            "{kind:?} {admissions:?} {caching}"
                        );
                    }
                }
            }
        }

        #[test]
        fn an_explicit_non_local_executor_is_never_admitted() {
            for executor in [hybrid(), none()] {
                for kind in [TestProviderKind::External, TestProviderKind::Internal] {
                    for admissions in [OFF, INTERNAL_CASES, WORKSPACE_TESTS, BOTH] {
                        assert_eq!(
                            admission(kind, Some(&executor), false, admissions),
                            LocalExecution::ByStrategy,
                            "{kind:?} {admissions:?} {executor:?}"
                        );
                    }
                }
            }
        }

        #[test]
        fn a_caching_external_test_is_refused_and_an_uncached_one_admitted() {
            assert_eq!(
                admission(
                    TestProviderKind::External,
                    Some(&local()),
                    true,
                    WORKSPACE_TESTS
                ),
                LocalExecution::ByStrategy
            );
            assert_eq!(
                admission(TestProviderKind::External, Some(&local()), true, BOTH),
                LocalExecution::ByStrategy
            );
            assert_eq!(
                admission(
                    TestProviderKind::External,
                    Some(&local()),
                    false,
                    WORKSPACE_TESTS
                ),
                LocalExecution::AdmittedLocalWorkspaceTest
            );
        }

        #[test]
        fn neither_option_admits_the_other_provider_kind() {
            // `local_internal_cases` never admits an External test.
            assert_eq!(
                admission(
                    TestProviderKind::External,
                    Some(&local()),
                    false,
                    INTERNAL_CASES
                ),
                LocalExecution::ByStrategy
            );
            // `local_workspace_tests` never admits an Internal test, cached or not.
            for caching in [false, true] {
                assert_eq!(
                    admission(
                        TestProviderKind::Internal,
                        Some(&local()),
                        caching,
                        WORKSPACE_TESTS
                    ),
                    LocalExecution::ByStrategy
                );
            }
            // Without either option nothing is admitted.
            for kind in [TestProviderKind::External, TestProviderKind::Internal] {
                assert_eq!(
                    admission(kind, Some(&local()), false, OFF),
                    LocalExecution::ByStrategy
                );
            }
        }

        #[test]
        fn internal_admission_is_unaffected() {
            for admissions in [INTERNAL_CASES, BOTH] {
                for caching in [false, true] {
                    assert_eq!(
                        admission(
                            TestProviderKind::Internal,
                            Some(&local()),
                            caching,
                            admissions
                        ),
                        LocalExecution::AdmittedInternalTest,
                        "{admissions:?} {caching}"
                    );
                }
            }
        }

        #[test]
        fn a_local_workspace_test_never_checks_or_uploads_a_cache() {
            for stage in [listing(), testing()] {
                for caching in [false, true] {
                    assert_eq!(
                        Orchestrator::test_command_cache_paths(
                            &stage,
                            caching,
                            LocalExecution::AdmittedLocalWorkspaceTest,
                        ),
                        (TestCachePath::NoOp, TestCachePath::NoOp),
                        "{stage:?} {caching}"
                    );
                }
            }
        }

        #[test]
        fn other_test_commands_keep_their_cache_paths() {
            for local_execution in [
                LocalExecution::ByStrategy,
                LocalExecution::AdmittedInternalTest,
            ] {
                assert_eq!(
                    Orchestrator::test_command_cache_paths(&listing(), false, local_execution),
                    (TestCachePath::Provided, TestCachePath::Provided)
                );
                assert_eq!(
                    Orchestrator::test_command_cache_paths(&testing(), true, local_execution),
                    (TestCachePath::Provided, TestCachePath::NoOp)
                );
                assert_eq!(
                    Orchestrator::test_command_cache_paths(&testing(), false, local_execution),
                    (TestCachePath::NoOp, TestCachePath::NoOp)
                );
            }
        }

        #[test]
        fn a_local_admission_key_is_a_closed_boolean() {
            for property in ["local_internal_cases", "local_workspace_tests"] {
                assert!(!Orchestrator::parse_local_admission(property, None).unwrap());
                assert!(!Orchestrator::parse_local_admission(property, Some("false")).unwrap());
                assert!(Orchestrator::parse_local_admission(property, Some("true")).unwrap());
                for invalid in ["", "True", "FALSE", "1", "0", "yes", "on", " true", "true "] {
                    let err =
                        Orchestrator::parse_local_admission(property, Some(invalid)).unwrap_err();
                    let message = format!("{err:?}");
                    assert!(
                        message.contains(&format!(
                            "[test] {property} must be `true` or `false`, not `{invalid}`"
                        )),
                        "{message}"
                    );
                }
            }
        }
    }

    /// Listing reuse through the real orchestration path: real analysis of a test rule's
    /// `ExternalRunnerTestInfo`, real `[test]` buckconfig, real DICE `TestExecutionKey`
    /// computation and executor admission, with only the command executor replaced by one that
    /// counts listing executions and returns the current bytes of an undeclared workspace file.
    mod listing_reuse {
        use std::path::Path;
        use std::path::PathBuf;
        use std::sync::Mutex;

        use buck2_build_api::actions::execute::dice_data::CommandExecutorResponse;
        use buck2_build_api::actions::execute::dice_data::HasCommandExecutor;
        use buck2_build_api::actions::execute::dice_data::SetCommandExecutor;
        use buck2_build_api::actions::execute::dice_data::SetReClient;
        use buck2_build_api::actions::execute::dice_data::set_fallback_executor_config;
        use buck2_build_api::actions::impls::run_action_knobs::RunActionKnobs;
        use buck2_build_api::build::detailed_aggregated_metrics::dice::SetDetailedAggregatedMetricsHandle;
        use buck2_build_api::build::detailed_aggregated_metrics::events::DetailedAggregatedMetricsHandle;
        use buck2_build_api::interpreter::rule_defs::command_executor_config::register_command_executor_config;
        use buck2_build_api::interpreter::rule_defs::register_rule_defs;
        use buck2_build_api::keep_going::HasKeepGoing;
        use buck2_build_api::spawner::BuckSpawner;
        use buck2_common::legacy_configs::configs::LegacyBuckConfig;
        use buck2_common::package_listing::listing::PackageListing;
        use buck2_common::package_listing::listing::testing::PackageListingExt;
        use buck2_configured::execution::ExecutionPlatformsKey;
        use buck2_core::build_file_path::BuildFilePath;
        use buck2_core::bzl::ImportPath;
        use buck2_core::cells::CellAliasResolver;
        use buck2_core::cells::cell_path::CellPath;
        use buck2_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
        use buck2_core::execution_types::executor_config::CommandExecutorConfig;
        use buck2_core::package::PackageLabel;
        use buck2_core::pattern::pattern::InferTargetNames;
        use buck2_execute::digest_config::SetDigestConfig;
        use buck2_execute::execute::action_digest::ActionDigest;
        use buck2_execute::execute::blocking::SetBlockingExecutor;
        use buck2_execute::execute::blocking::testing::DummyBlockingExecutor;
        use buck2_execute::execute::cache_uploader::NoOpCacheUploader;
        use buck2_execute::execute::kind::CommandExecutionKind;
        use buck2_execute::execute::manager::CommandExecutionManager;
        use buck2_execute::execute::output::CommandStdStreams;
        use buck2_execute::execute::prepared::NoOpCommandOptionalExecutor;
        use buck2_execute::execute::prepared::PreparedCommand;
        use buck2_execute::execute::prepared::PreparedCommandExecutor;
        use buck2_execute::execute::request::ExecutorPreference;
        use buck2_execute::execute::result::CommandExecutionMetadata;
        use buck2_execute::execute::result::CommandExecutionResult;
        use buck2_execute::materialize::materializer::SetMaterializer;
        use buck2_execute::re::manager::UnconfiguredRemoteExecutionClient;
        use buck2_execute_impl::materializers::deferred::NoDiskDeferredMaterializer;
        use buck2_hash::BuckFixedInsertionMap;
        use buck2_hash::IntentionallyStdHashMap;
        use buck2_interpreter::dice::starlark_debug::SetStarlarkDebugger;
        use buck2_interpreter::extra::InterpreterHostArchitecture;
        use buck2_interpreter::extra::InterpreterHostPlatform;
        use buck2_interpreter::file_loader::LoadedModules;
        use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
        use buck2_interpreter_for_build::interpreter::calculation::InterpreterResultsKey;
        use buck2_interpreter_for_build::interpreter::configuror::BuildInterpreterConfiguror;
        use buck2_interpreter_for_build::interpreter::dice_calculation_delegate::testing::EvalImportKey;
        use buck2_interpreter_for_build::interpreter::interpreter_setup::setup_interpreter_basic;
        use buck2_interpreter_for_build::interpreter::testing::Tester;
        use buck2_interpreter_for_build::rule::register_rule_function;
        use buck2_node::package_visibility::HasPackageVisibilityDefaultIntersection;
        use buck2_resource_control::HasResourceControl;
        use buck2_settings::PackageVisibilityDefaultIntersection;
        use buck2_test_api::data::ArgValueContent;
        use buck2_test_api::data::ExternalRunnerSpecValue;
        use buck2_util::time_span::TimeSpan;
        use indoc::indoc;

        use super::*;

        const RULES: &str = indoc!(
            r#"
            def _local():
                return CommandExecutorConfig(local_enabled = True, remote_enabled = False)

            def _impl(ctx):
                mode = ctx.attrs.mode
                if mode == "default_local" or mode == "cache_capable":
                    executors = {"default_executor": _local()}
                elif mode == "listing_override":
                    executors = {"executor_overrides": {"listing": _local()}}
                elif mode == "named_override":
                    executors = {"executor_overrides": {"named": _local()}}
                elif mode == "platform_fallback":
                    executors = {}
                else:
                    fail("unknown mode " + mode)
                return [
                    DefaultInfo(),
                    ExternalRunnerTestInfo(
                        type = "custom",
                        command = ["discover"],
                        supports_test_execution_caching = mode == "cache_capable",
                        **executors
                    ),
                ]

            live_test = rule(impl = _impl, attrs = {"mode": attrs.string()})

            TestRun = exec_group()

            def _parse_listing(stdout):
                return [{"name": name, "filter": name} for name in stdout.split("\n") if name]

            def _parse_result(stdout, stderr, exit_code):
                return []

            def _internal(**executors):
                return InternalRunnerTestInfo(
                    type = "custom",
                    command = ["discover"],
                    listing_command = ["discover"],
                    parse_test_listing = _parse_listing,
                    parse_test_result = _parse_result,
                    **executors
                )

            # Names an execution group and passes no `default_executor`: analysis records the
            # group's execution platform executor, which here is the local fallback platform's.
            def _group_impl(ctx):
                group = TestRun if ctx.attrs.named else DefaultExecGroup
                if ctx.attrs.internal:
                    info = _internal(exec_group = group)
                else:
                    info = ExternalRunnerTestInfo(
                        type = "custom",
                        command = ["discover"],
                        exec_group = group,
                    )
                return [DefaultInfo(), info]

            group_test = rule(
                impl = _group_impl,
                attrs = {"internal": attrs.bool(), "named": attrs.bool()},
                exec_groups = [DefaultExecGroup, TestRun],
            )

            # Re-creates the test info of `test` field by field, as upstream supports: the recorded
            # executor is passed on as `default_executor`.
            def _copying_impl(ctx):
                if ctx.attrs.internal:
                    info = ctx.attrs.test[InternalRunnerTestInfo]
                    copy = _internal(
                        default_executor = info.default_executor,
                        exec_group = info.exec_group,
                    )
                else:
                    info = ctx.attrs.test[ExternalRunnerTestInfo]
                    copy = ExternalRunnerTestInfo(
                        type = info.test_type,
                        command = info.command,
                        default_executor = info.default_executor,
                        exec_group = info.exec_group,
                    )
                return [DefaultInfo(), copy]

            copying_test = rule(
                impl = _copying_impl,
                attrs = {"internal": attrs.bool(), "test": attrs.dep()},
            )

            # The explicit positive control for Internal tests: a declared local executor.
            internal_local_test = rule(
                impl = lambda _ctx: [DefaultInfo(), _internal(default_executor = _local())],
                attrs = {},
            )
            "#
        );

        const BUILD: &str = indoc!(
            r#"
            load(":rules.bzl", "copying_test", "group_test", "internal_local_test", "live_test")

            live_test(name = "default_local", mode = "default_local")
            live_test(name = "cache_capable", mode = "cache_capable")
            live_test(name = "listing_override", mode = "listing_override")
            live_test(name = "named_override", mode = "named_override")
            live_test(name = "platform_fallback", mode = "platform_fallback")
            group_test(name = "group_default", internal = False, named = False)
            group_test(name = "group_named", internal = False, named = True)
            group_test(name = "internal_group_default", internal = True, named = False)
            group_test(name = "internal_group_named", internal = True, named = True)
            copying_test(name = "group_copied", internal = False, test = ":group_named")
            copying_test(name = "internal_group_copied", internal = True, test = ":internal_group_named")
            internal_local_test(name = "internal_default_local")
            "#
        );

        /// The undeclared live-workspace file the listing reads, relative to the project root.
        const LIVE_FILE: &str = "cell/pkg/live_cases.txt";

        fn init_late_bindings() {
            crate::testing::init_late_bindings();
        }

        /// Runs every listing by reading the live file's current bytes as its stdout, and records
        /// each execution.
        struct LiveFileListing {
            live_file: PathBuf,
            executions: Arc<Mutex<Vec<Vec<u8>>>>,
        }

        #[async_trait]
        impl PreparedCommandExecutor for LiveFileListing {
            async fn exec_cmd(
                &self,
                command: &PreparedCommand<'_, '_>,
                manager: CommandExecutionManager,
                _cancellations: &CancellationContext,
            ) -> CommandExecutionResult {
                let manager = manager.claim().await;
                let stdout = std::fs::read(&self.live_file).expect("live file is readable");
                self.executions.lock().unwrap().push(stdout.clone());
                manager.success(
                    CommandExecutionKind::Local {
                        digest: ActionDigest::empty(command.digest_config.cas_digest_config()),
                        command: Default::default(),
                        env: Default::default(),
                    },
                    BuckMutInsertionMap::default(),
                    CommandStdStreams::Local {
                        stdout,
                        stderr: Vec::new(),
                    },
                    CommandExecutionMetadata::empty(TimeSpan::empty_now()),
                )
            }

            fn is_local_execution_possible(
                &self,
                _executor_preference: ExecutorPreference,
            ) -> bool {
                false
            }
        }

        /// Hands out the counting executor, uncached, and records the admission of every command
        /// it is asked for.
        struct Executors {
            live_file: PathBuf,
            executions: Arc<Mutex<Vec<Vec<u8>>>>,
            admissions: Arc<Mutex<Vec<LocalExecution>>>,
        }

        impl HasCommandExecutor for Executors {
            fn get_command_executor(
                &self,
                _artifact_fs: &ArtifactFs,
                _config: &CommandExecutorConfig,
                local_execution: LocalExecution,
            ) -> buck2_error::Result<CommandExecutorResponse> {
                self.admissions.lock().unwrap().push(local_execution);
                Ok(CommandExecutorResponse {
                    executor: Arc::new(LiveFileListing {
                        live_file: self.live_file.clone(),
                        executions: self.executions.dupe(),
                    }),
                    action_cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                    remote_dep_file_cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                    platform: Default::default(),
                    cache_uploader: Arc::new(NoOpCacheUploader {}),
                    output_trees_download_config:
                        buck2_execute::re::output_trees_download_config::OutputTreesDownloadConfig::new(
                            None, true,
                        ),
                    write_action_cache: None,
                })
            }
        }

        /// What two identical cacheable listing requests observed.
        #[derive(Debug, PartialEq, Eq)]
        struct Observed {
            /// The stdout each `prepare_and_execute` call returned.
            returned: [Vec<u8>; 2],
            /// The bytes each actual listing execution read.
            executions: Vec<Vec<u8>>,
            /// The admission of every executor the orchestrator prepared.
            admissions: Vec<LocalExecution>,
        }

        /// One test target, one DICE transaction and two identical `Listing { cacheable: true }`
        /// requests through `prepare_and_execute`, with the live file changed between them.
        /// `local_opt_ins` sets both `[test] local_workspace_tests` and `local_internal_cases`;
        /// each admits only its own provider kind.
        async fn list_twice(
            mode: &str,
            local_opt_ins: bool,
            executor_override: Option<&str>,
            disable_test_execution_caching: bool,
        ) -> buck2_error::Result<Observed> {
            init_late_bindings();
            let bzlfile = ImportPath::testing_new("cell//pkg:rules.bzl");
            let resolver = CellResolver::testing_with_names_and_paths(&[
                (
                    CellName::testing_new("root"),
                    CellRootPathBuf::testing_new(""),
                ),
                (
                    CellName::testing_new("cell"),
                    CellRootPathBuf::testing_new("cell"),
                ),
            ]);
            let mut interpreter = Tester::with_cells((
                CellAliasResolver::new(
                    CellName::testing_new("cell"),
                    IntentionallyStdHashMap::new(),
                )?,
                resolver.dupe(),
                LegacyBuckConfig::empty(),
                CellPathWithAllowedRelativeDir::new(CellPath::testing_new("cell//pkg"), None),
            ))?;
            interpreter.additional_globals(register_rule_function);
            interpreter.additional_globals(register_rule_defs);
            interpreter.additional_globals(register_command_executor_config);
            let module = interpreter.eval_import(&bzlfile, RULES, LoadedModules::default())?;
            let eval_res = interpreter.eval_build_file_with_loaded_modules(
                &BuildFilePath::testing_new("cell//pkg:BUCK"),
                BUILD,
                LoadedModules {
                    map: BuckFixedInsertionMap::from_iter([(
                        OwnedStarlarkModulePath::LoadFile(bzlfile.clone()),
                        module.dupe(),
                    )]),
                },
                PackageListing::testing_new(&[], "BUCK"),
            )?;

            let fs = ProjectRootTemp::new()?;
            let live_file = AsRef::<Path>::as_ref(fs.path().root()).join(LIVE_FILE);
            std::fs::create_dir_all(live_file.parent().unwrap())?;
            std::fs::write(&live_file, "case_a\n")?;

            let executions = Arc::new(Mutex::new(Vec::new()));
            let admissions = Arc::new(Mutex::new(Vec::new()));
            let mut dice = DiceBuilder::new()
                .mock_and_return(
                    EvalImportKey(OwnedStarlarkModulePath::LoadFile(bzlfile.clone())),
                    Ok(module),
                )
                .mock_and_return(
                    InterpreterResultsKey(PackageLabel::testing_parse("cell//pkg")),
                    Ok(Arc::new(eval_res)),
                )
                .mock_and_return(ExecutionPlatformsKey, Ok(None))
                .set_data(|data| {
                    data.set_testing_io_provider(&fs);
                    data.set_digest_config(DigestConfig::testing_default());
                    data.set_detailed_aggregated_metrics_handle(
                        DetailedAggregatedMetricsHandle::new(),
                    );
                })
                .build({
                    let mut data = UserComputationData::new();
                    data.set_keep_going(true);
                    data.set_starlark_debugger_handle(None);
                    data.set_package_visibility_default_intersection(
                        PackageVisibilityDefaultIntersection::Off,
                    );
                    set_fallback_executor_config(
                        &mut data.data,
                        CommandExecutorConfig::testing_local(),
                    );
                    data.set_command_executor(Box::new(Executors {
                        live_file: live_file.clone(),
                        executions: executions.dupe(),
                        admissions: admissions.dupe(),
                    }));
                    data.set_blocking_executor(Arc::new(DummyBlockingExecutor {
                        fs: fs.path().dupe(),
                    }));
                    data.set_materializer(Arc::new(
                        NoDiskDeferredMaterializer::testing_new_no_disk(fs.path().dupe())?,
                    ));
                    data.set_re_client(UnconfiguredRemoteExecutionClient::testing_new_dummy());
                    data.data.set(EventDispatcher::null());
                    data.data.set(RunActionKnobs::default());
                    data.data.set(HasResourceControl(false));
                    data.spawner = Arc::new(BuckSpawner::current_runtime().unwrap());
                    data
                })
                .await
                .unwrap();
            setup_interpreter_basic(
                &mut dice,
                resolver,
                BuildInterpreterConfiguror::new(
                    None,
                    InterpreterHostPlatform::Linux,
                    InterpreterHostArchitecture::X86_64,
                    None,
                    false,
                    false,
                    InferTargetNames::No,
                    None,
                )?,
            )?;
            // The root cell's real `.buckconfig`, read through the project's file operations.
            let root = AsRef::<Path>::as_ref(fs.path().root());
            std::fs::write(
                root.join(".buckconfig"),
                if local_opt_ins {
                    "[test]\nlocal_workspace_tests = true\nlocal_internal_cases = true\n"
                } else {
                    ""
                },
            )?;
            std::fs::write(root.join("cell/.buckconfig"), "")?;
            dice.set_buck_out_path(Some(ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2".to_owned(),
            )))?;
            let dice = dice.commit().await;

            let key = TestExecutionKey {
                test_target: ConfiguredProvidersLabel::new(
                    ConfiguredTargetLabel::testing_parse(
                        &format!("cell//pkg:{mode}"),
                        ConfigurationData::testing_new(),
                    ),
                    Default::default(),
                ),
                cmd: Arc::new(vec![ArgValue {
                    content: ArgValueContent::ExternalRunnerSpecValue(
                        ExternalRunnerSpecValue::Verbatim("discover".to_owned()),
                    ),
                    format: None,
                }]),
                env: Arc::new(BuckFixedSortedMap::new()),
                executor_override: executor_override.map(|name| {
                    Arc::new(ExecutorConfigOverride {
                        name: name.to_owned(),
                    })
                }),
                required_local_resources: Arc::new(RequiredLocalResources {
                    resources: Vec::new(),
                }),
                pre_create_dirs: Arc::new(Vec::new()),
                stage: Arc::new(TestStage::Listing {
                    suite: mode.to_owned(),
                    cacheable: true,
                }),
                options: TestSessionOptions::default(),
                timeout: Duration::from_secs(60),
                host_sharing_requirements: Arc::new(HostSharingRequirements::default()),
                disable_test_execution_caching,
            };
            let mut returned = Vec::new();
            for live in ["case_a\n", "case_a\ncase_b\n"] {
                // The file changes only between completed requests; the request, its key and the
                // DICE version stay identical.
                std::fs::write(&live_file, live)?;
                let data = match prepare_and_execute(
                    &mut dice.dupe().ctx(),
                    CancellationContext::testing(),
                    key.dupe(),
                    NoopLivelinessObserver::create(),
                    &InternalRunnerConfig::parse(None),
                    None,
                )
                .await
                {
                    Ok(data) => data,
                    Err(ExecuteError::Error(e)) => return Err(e),
                    Err(ExecuteError::Cancelled(_)) => panic!("listing was cancelled"),
                };
                assert_eq!(data.status, ExecutionStatus::Finished { exitcode: 0 });
                let ExecutionStream::Inline(stdout) = data.stdout;
                returned.push(stdout);
            }
            let returned: [Vec<u8>; 2] = returned.try_into().unwrap();
            let executions = executions.lock().unwrap().clone();
            let admissions = admissions.lock().unwrap().clone();
            Ok(Observed {
                returned,
                executions,
                admissions,
            })
        }

        /// Both requests execute and the second returns the changed file.
        fn fresh(admission: LocalExecution) -> Observed {
            Observed {
                returned: [b"case_a\n".to_vec(), b"case_a\ncase_b\n".to_vec()],
                executions: vec![b"case_a\n".to_vec(), b"case_a\ncase_b\n".to_vec()],
                admissions: vec![admission, admission],
            }
        }

        /// The first request executes and the second reuses its DICE result.
        fn reused() -> Observed {
            Observed {
                returned: [b"case_a\n".to_vec(), b"case_a\n".to_vec()],
                executions: vec![b"case_a\n".to_vec()],
                admissions: vec![LocalExecution::ByStrategy],
            }
        }

        #[tokio::test]
        async fn admitted_local_workspace_listing_runs_for_every_request() -> buck2_error::Result<()>
        {
            assert_eq!(
                list_twice("default_local", true, None, false).await?,
                fresh(LocalExecution::AdmittedLocalWorkspaceTest)
            );
            Ok(())
        }

        #[tokio::test]
        async fn admitted_listing_override_and_named_override_run_for_every_request()
        -> buck2_error::Result<()> {
            assert_eq!(
                list_twice("listing_override", true, None, false).await?,
                fresh(LocalExecution::AdmittedLocalWorkspaceTest)
            );
            assert_eq!(
                list_twice("named_override", true, Some("named"), false).await?,
                fresh(LocalExecution::AdmittedLocalWorkspaceTest)
            );
            Ok(())
        }

        #[tokio::test]
        async fn ordinary_cacheable_listing_reuses_its_dice_result() -> buck2_error::Result<()> {
            // Without the root opt-in, the same explicitly local uncached provider is no
            // local-workspace test, and its successful cacheable listing is reused.
            assert_eq!(
                list_twice("default_local", false, None, false).await?,
                reused()
            );
            Ok(())
        }

        #[tokio::test]
        async fn unadmitted_listings_reuse_their_dice_result_under_the_opt_in()
        -> buck2_error::Result<()> {
            // A cache-capable provider is refused even when the request disables caching.
            assert_eq!(
                list_twice("cache_capable", true, None, true).await?,
                reused()
            );
            // A platform-derived fallback executor is never admitted.
            assert_eq!(
                list_twice("platform_fallback", true, None, false).await?,
                reused()
            );
            // With executor overrides but no `listing` override or default, a listing has no
            // provider-selected executor either.
            assert_eq!(
                list_twice("named_override", true, None, false).await?,
                reused()
            );
            Ok(())
        }

        /// S1-PROVIDER-01. A test info that names an execution group and passes no
        /// `default_executor` records the group's execution platform executor during analysis.
        /// Here that platform is local, both opt-ins are on, and the provider is uncached; still
        /// the executor was chosen by execution platform resolution, not by the rule, so the
        /// command is not admitted and the factory's `--remote-only` ban applies
        /// (`LocalExecution::ByStrategy`). This covers `DefaultExecGroup` and a named group, both
        /// provider kinds, and a field-by-field copy of the test info in a rule that declares no
        /// group.
        #[tokio::test]
        async fn an_execution_group_executor_is_not_admitted_under_the_opt_ins()
        -> buck2_error::Result<()> {
            let mut admitted = Vec::new();
            for mode in [
                "group_default",
                "group_named",
                "group_copied",
                "internal_group_default",
                "internal_group_named",
                "internal_group_copied",
            ] {
                let observed = list_twice(mode, true, None, false).await?;
                if observed != reused() {
                    admitted.push((mode, observed.admissions));
                }
            }
            assert_eq!(admitted, Vec::new());
            Ok(())
        }

        /// The explicit positive controls next to S1-PROVIDER-01: the same local executor,
        /// declared with `CommandExecutorConfig(...)`, is admitted for each provider kind.
        #[tokio::test]
        async fn a_declared_local_executor_is_admitted_for_both_provider_kinds()
        -> buck2_error::Result<()> {
            assert_eq!(
                list_twice("default_local", true, None, false).await?,
                fresh(LocalExecution::AdmittedLocalWorkspaceTest)
            );
            // An admitted Internal listing still reuses its successful cacheable result.
            assert_eq!(
                list_twice("internal_default_local", true, None, false).await?,
                Observed {
                    returned: [b"case_a\n".to_vec(), b"case_a\n".to_vec()],
                    executions: vec![b"case_a\n".to_vec()],
                    admissions: vec![LocalExecution::AdmittedInternalTest],
                }
            );
            Ok(())
        }
    }
}
