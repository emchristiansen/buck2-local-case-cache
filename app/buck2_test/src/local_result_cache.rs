/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Local reuse of semantically confirmed successful exact test cases.
//!
//! An `InternalRunnerTestInfo` provider requests reuse with
//! `supports_test_execution_caching = True`. The request is necessary and never sufficient. A
//! Testing request takes part only when every one of these holds:
//!
//! - `[test] local_case_result_cache_root` names an absolute directory owned by this user and
//!   inaccessible to others; no default location exists. The path must be lexically canonical,
//!   no component may be a symlink, and no ancestor may be changeable by another principal
//!   (owned by root or this user and not group- or world-writable, unless root-owned and
//!   sticky). The directory first found is pinned by descriptor for the daemon's life; if the
//!   spelling later names another directory, opted-in cases are refused until a restart;
//! - the request runs exactly one listed case, with no variant or repeat;
//! - the child's whole environment is decided by the request: the inheritance clears the daemon
//!   environment, there is no scratch path, worker or local resource, and the per-invocation
//!   identity variables are not injected (`determined_local_environment`);
//! - the request declares no outputs, every file its command names (`argv[0]`, a launched
//!   script's `#!` interpreter, every `PATH` entry, and any argument or environment value naming
//!   an existing file) is in the input tree or the Nix store, every input-tree file is a build
//!   artifact under `buck-out` rather than a checkout file, every input-tree symlink reaches a
//!   declared build artifact or a Nix store path, and no loader override is set
//!   (`local_case_closure`); a literal host tool such as `/bin/sh` makes the case ineligible;
//! - the executor can and must run it locally.
//!
//! Any other opted-in request executes normally and publishes nothing.
//!
//! The key is a length-delimited record of the full configured target, suite and case, the
//! prepared action digest (argv, request environment, working directory, timeout, platform and
//! the whole input tree), every argument, the absolute working directory, every installed
//! environment byte in installation order (including `TMPDIR` and `PWD`, and absence by
//! omission), the null stdin, the network policy, the effective user and the host platform.
//! Nothing is normalised: an entry is reused only for byte-identical key material. The record is
//! stored beside the entry and compared in full on every access.
//!
//! A reused result asserts that an earlier execution of the identical contract passed. It does not
//! assert that a fresh execution of a nondeterministic test would pass; the provider's explicit
//! request is its declaration that the case is deterministic under the declared contract.
//!
//! A waiter for a key's lease holds no blocking thread, so a holder's settlement, which runs on
//! the blocking pool, cannot be starved by waiters for the same lease.
//!
//! Publication is one use of a capability: a durable pending generation is written before the
//! child is dispatched, the child's exact raw streams are captured only for a local exit 0, and the
//! entry is committed only after the in-process parser returns exactly one PASS for the expected
//! case and that result is reported. A newer generation that is pending, unsuccessful or abandoned
//! hides every older success, so a failed or interrupted rerun never resurrects an old pass.

use std::ffi::OsString;
use std::fmt;
use std::time::Duration;
use std::time::SystemTime;

use buck2_common::cas_digest::CasDigestConfig;
use buck2_common::cas_digest::DigestAlgorithm;
use buck2_common::file_ops::metadata::FileDigest;
use buck2_execute::execute::action_digest::ActionDigest;
use buck2_execute::execute::kind::CommandExecutionKind;
use buck2_execute::execute::output::CommandStdStreams;
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_execute::execute::result::CommandExecutionReport;
use buck2_execute::execute::result::CommandExecutionStatus;
use buck2_core::fs::project::ProjectRoot;
use buck2_execute_impl::executors::local::determined_local_environment;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_test_api::data::TestStage;
use buck2_test_api::data::TestStatus;
use serde::Deserialize;
use serde::Serialize;

use crate::local_case_closure::InputTree;
use crate::local_case_fresh_dirs::FreshDirs;
use crate::local_case_fresh_dirs::ScratchLease;
use crate::local_case_closure::LaunchFacts;
use crate::local_case_closure::LaunchPlan;

/// Version of the key record, the on-disk layout and the publication protocol. Any change to
/// what the key covers or how entries are confirmed must change this value.
pub(crate) const LOCAL_CASE_RESULT_SCHEMA: &str = "buck2-local-test-case-result-v1";

/// Why an opted-in Testing request executes without reuse or publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalCaseGap {
    NoStoreConfigured,
    NotOneExactCase,
    LocalResources,
    DeclaredOutputs,
    Worker,
    ScratchPath,
    InheritedDaemonEnvironment,
    /// An input is a checkout file, or a file the command names is neither an input-tree
    /// build artifact nor a Nix store path.
    UnboundCommandFile(String),
    LocalExecutionImpossible,
    NoDiscoveryIdentity,
    ParserNotSelfContained,
    EngineIdentityUnavailable,
}

impl fmt::Display for LocalCaseGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoStoreConfigured => "no [test] local_case_result_cache_root is configured",
            Self::NotOneExactCase => "the request is not exactly one case without variant or repeat",
            Self::LocalResources => "the case requires local resources",
            Self::DeclaredOutputs => "the case declares outputs",
            Self::Worker => "the case runs in a worker",
            Self::ScratchPath => "the case has a scratch path",
            Self::InheritedDaemonEnvironment => {
                "the child environment is not fully decided by the request"
            }
            Self::UnboundCommandFile(why) => return write!(f, "the command is not bound: {why}"),
            Self::LocalExecutionImpossible => "the executor cannot run the case locally",
            Self::NoDiscoveryIdentity => "the request did not come from internal-runner discovery",
            Self::ParserNotSelfContained => {
                "the provider's parsers are not self-contained source-bound functions"
            }
            Self::EngineIdentityUnavailable => "the running Buck2 executable could not be digested",
        })
    }
}

/// The single case a Testing request runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaseIdentity {
    target: String,
    suite: String,
    case: String,
    discovery: String,
}

/// Identity of the provider contract and discovery that produced a case: the provider type,
/// its run and listing specs, its parser callables and the complete raw listing output.
///
/// The parser's behaviour is enforced again on every reuse, because a reused result is
/// reported only if the current parser confirms exactly one PASS for the case from the original
/// raw streams. The callable representation recorded here is therefore a conservative
/// invalidation input, not the parser's semantic identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryIdentity(String);

impl DiscoveryIdentity {
    pub(crate) fn new(
        test_type: &str,
        run_spec: &str,
        listing_spec: &str,
        parsers: &str,
        listing_stdout: &[u8],
    ) -> Self {
        let mut record = KeyRecord::default();
        record.field("schema", LOCAL_CASE_RESULT_SCHEMA.as_bytes());
        record.field("test_type", test_type.as_bytes());
        record.field("run_spec", run_spec.as_bytes());
        record.field("listing_spec", listing_spec.as_bytes());
        record.field("parsers", parsers.as_bytes());
        record.field("listing_stdout", listing_stdout);
        Self(sha256(&record.0))
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    FileDigest::from_content_for_algorithm(bytes, DigestAlgorithm::Sha256)
        .raw_digest()
        .to_string()
}

/// What the internal runner asks of the local store for one case of an opted-in provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalCaseRequest {
    /// The provider's contract is not supported; the case executes normally.
    Unsupported(LocalCaseGap),
    /// Record the case as a generation.
    Track {
        discovery: DiscoveryIdentity,
        /// Whether a committed success may be reused. A rerun after a reused result was not
        /// confirmed is still a tracked generation, but never reads the store.
        reuse: bool,
    },
}

/// SHA-256 of the running Buck2 executable, computed once per daemon. Any engine change, and so
/// any change to how this module keys, executes or confirms a case, invalidates every entry.
pub(crate) fn engine_identity() -> Result<&'static str, LocalCaseGap> {
    static ENGINE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ENGINE
        .get_or_init(|| {
            // On Linux this names the running image even if the file was since replaced.
            let path = if cfg!(target_os = "linux") {
                std::path::PathBuf::from("/proc/self/exe")
            } else {
                std::env::current_exe().ok()?
            };
            let file = std::fs::File::open(path).ok()?;
            FileDigest::from_reader_for_algorithm(file, DigestAlgorithm::Sha256)
                .ok()
                .map(|digest| digest.raw_digest().to_string())
        })
        .as_deref()
        .ok_or(LocalCaseGap::EngineIdentityUnavailable)
}

/// Where an opted-in Testing request stands before its child is prepared.
pub(crate) enum LocalCaseAdmission {
    /// The provider did not request reuse, or the request disabled test execution caching.
    NotRequested,
    /// Requested, but the provider's contract is unsupported or no store is configured: the case
    /// executes normally, is not recorded and publishes nothing.
    Ineligible(LocalCaseGap),
    /// Requested and configured, but the request cannot be recorded as a generation; it must
    /// not execute.
    Refused(LocalCaseGap),
    /// Requested, configured and naming one exact case; the request itself is checked next.
    Candidate(LocalCaseCandidate),
}

pub(crate) struct LocalCaseCandidate {
    root: String,
    reuse: bool,
    case: CaseIdentity,
}

impl LocalCaseAdmission {
    pub(crate) fn for_stage(
        stage: &TestStage,
        target: &str,
        provider_requested: bool,
        disabled: bool,
        root: Option<&str>,
        request: Option<&LocalCaseRequest>,
    ) -> Self {
        let TestStage::Testing {
            suite,
            testcases,
            variant,
            repeat_count,
        } = stage
        else {
            return Self::NotRequested;
        };
        if !provider_requested {
            return Self::NotRequested;
        }
        let Some(root) = root else {
            return Self::Ineligible(LocalCaseGap::NoStoreConfigured);
        };
        let (discovery, reuse) = match request {
            // An opted-in case requested other than by the internal runner could not be
            // recorded, so it must not run beside the store's generations.
            None => return Self::Refused(LocalCaseGap::NoDiscoveryIdentity),
            Some(LocalCaseRequest::Unsupported(gap)) => return Self::Ineligible(gap.clone()),
            Some(LocalCaseRequest::Track { discovery, reuse }) => (discovery, *reuse),
        };
        let [case] = testcases.as_slice() else {
            return Self::Refused(LocalCaseGap::NotOneExactCase);
        };
        if variant.is_some() || repeat_count.is_some() {
            return Self::Refused(LocalCaseGap::NotOneExactCase);
        }
        Self::Candidate(LocalCaseCandidate {
            root: root.to_owned(),
            // A request that disables caching still runs as a tracked generation: it never
            // reads the store, and its pending record hides every older success.
            reuse: reuse && !disabled,
            case: CaseIdentity {
                target: target.to_owned(),
                suite: suite.clone(),
                case: case.clone(),
                discovery: discovery.0.clone(),
            },
        })
    }
}

/// Where the request runs, to classify the files its command names.
pub(crate) struct RequestFacts<'a> {
    pub(crate) project_fs: &'a ProjectRoot,
    /// Project-relative components of `buck-out`.
    pub(crate) buck_out: Vec<String>,
}

impl LocalCaseCandidate {
    /// Check the expanded request and, if it qualifies, constrain it to the reuse contract:
    /// local execution only, and no injected per-invocation identity variables.
    ///
    /// The request is returned unchanged when it does not qualify.
    ///
    /// The only scratch path admitted is the engine's own fresh-directory scratch path for this
    /// request, `fresh_dirs`, exactly as the engine set it up.
    pub(crate) fn constrain(
        self,
        request: CommandExecutionRequest,
        facts: RequestFacts<'_>,
        fresh_dirs: Option<&FreshDirs>,
        local_execution_possible: impl FnOnce(ExecutorPreference) -> bool,
    ) -> (
        CommandExecutionRequest,
        Result<ConstrainedCase, LocalCaseGap>,
    ) {
        let gap = if !request.required_local_resources().is_empty() {
            Some(LocalCaseGap::LocalResources)
        } else if request.outputs().next().is_some() {
            Some(LocalCaseGap::DeclaredOutputs)
        } else if request.worker().is_some() {
            Some(LocalCaseGap::Worker)
        } else if request.inputs().iter().any(|input| {
            matches!(
                input,
                buck2_execute::execute::request::CommandExecutionInput::ScratchPath(..)
            )
        }) && !fresh_dirs.is_some_and(|fresh_dirs| fresh_dirs.admits(&request))
        {
            Some(LocalCaseGap::ScratchPath)
        } else if !request
            .local_environment_inheritance()
            .is_some_and(|inheritance| inheritance.clear())
        {
            Some(LocalCaseGap::InheritedDaemonEnvironment)
        } else {
            None
        };
        if let Some(gap) = gap {
            return (request, Err(gap));
        }
        let Ok(preference) = request
            .executor_preference()
            .and(ExecutorPreference::LocalRequired)
        else {
            return (request, Err(LocalCaseGap::LocalExecutionImpossible));
        };
        if !local_execution_possible(preference) {
            return (request, Err(LocalCaseGap::LocalExecutionImpossible));
        }
        let legacy = LegacyRequest {
            executor_preference: request.executor_preference(),
            omit_local_invocation_identity_env: request.omit_local_invocation_identity_env(),
        };
        let request = request.with_omit_local_invocation_identity_env(true);
        let scratch = fresh_dirs.map(|fresh_dirs| fresh_dirs.absolute().clone());
        let launch = match Self::launch_plan(&request, &facts, scratch.as_deref().map(AsRef::<AbsPath>::as_ref)) {
            Ok(launch) => launch,
            Err(gap) => return (legacy.restore(request), Err(gap)),
        };
        let mut request = request;
        request.executor_preference = preference;
        (
            request,
            Ok(ConstrainedCase {
                root: self.root,
                reuse: self.reuse,
                case: self.case,
                launch,
                legacy,
                scratch,
            }),
        )
    }

    /// Classify every file the constrained request's command names.
    fn launch_plan(
        request: &CommandExecutionRequest,
        facts: &RequestFacts<'_>,
        scratch: Option<&AbsPath>,
    ) -> Result<LaunchPlan, LocalCaseGap> {
        let working_directory = facts.project_fs.resolve(request.working_directory());
        let env = determined_local_environment(request, &working_directory, scratch)
            .ok_or(LocalCaseGap::InheritedDaemonEnvironment)?;
        let tree = InputTree::new(request.paths().input_directory());
        LaunchPlan::new(
            &tree,
            LaunchFacts {
                project_root: facts.project_fs.root().as_path(),
                working_directory: request
                    .working_directory()
                    .iter()
                    .map(|c| c.as_str().to_owned())
                    .collect(),
                buck_out: facts.buck_out.clone(),
                argv: &request.all_args_vec(),
                env: &env,
            },
        )
        .map_err(LocalCaseGap::UnboundCommandFile)
    }
}

/// What `constrain` changes in a request, as it was before.
struct LegacyRequest {
    executor_preference: ExecutorPreference,
    omit_local_invocation_identity_env: bool,
}

impl LegacyRequest {
    fn restore(&self, request: CommandExecutionRequest) -> CommandExecutionRequest {
        let mut request = request
            .with_omit_local_invocation_identity_env(self.omit_local_invocation_identity_env);
        request.executor_preference = self.executor_preference;
        request
    }
}

/// An eligible case whose request has been constrained to the reuse contract.
pub(crate) struct ConstrainedCase {
    root: String,
    reuse: bool,
    case: CaseIdentity,
    launch: LaunchPlan,
    legacy: LegacyRequest,
    /// The engine's fresh-directory scratch path, absolute, if the request has one.
    scratch: Option<AbsNormPathBuf>,
}

/// Facts about the prepared action that enter the key.
pub(crate) struct PreparedFacts<'a> {
    pub(crate) action_digest: &'a ActionDigest,
    pub(crate) working_directory: &'a AbsPath,
    pub(crate) network_access: String,
    pub(crate) digest_config: CasDigestConfig,
}

impl ConstrainedCase {
    pub(crate) fn root(&self) -> &str {
        &self.root
    }

    pub(crate) fn reuse(&self) -> bool {
        self.reuse
    }

    /// The input-tree executables the command launches.
    pub(crate) fn launch(&self) -> &LaunchPlan {
        &self.launch
    }

    /// Give up the case after a later check found it ineligible, returning `request` exactly as
    /// it was before `constrain`: its own executor preference, and the per-invocation identity
    /// variables injected as for any request. The case then executes as if reuse had not been
    /// requested, and nothing is recorded or published.
    pub(crate) fn into_legacy_request(
        self,
        request: CommandExecutionRequest,
    ) -> CommandExecutionRequest {
        self.legacy.restore(request)
    }

    /// Build the full key for the constrained request.
    pub(crate) fn key(
        &self,
        request: &CommandExecutionRequest,
        prepared: PreparedFacts<'_>,
    ) -> Result<LocalCaseKey, LocalCaseGap> {
        let environment =
            determined_local_environment(
                request,
                prepared.working_directory,
                self.scratch.as_deref().map(AsRef::<AbsPath>::as_ref),
            )
            .ok_or(LocalCaseGap::InheritedDaemonEnvironment)?;
        Ok(LocalCaseKey::new(KeyMaterial {
            engine: engine_identity()?,
            case: &self.case,
            action_digest: prepared.action_digest,
            argv: &request.all_args_vec(),
            working_directory: prepared.working_directory,
            environment: &environment,
            timeout: request.timeout(),
            network_access: &prepared.network_access,
            digest_config: prepared.digest_config,
        }))
    }
}

struct KeyMaterial<'a> {
    engine: &'a str,
    case: &'a CaseIdentity,
    action_digest: &'a ActionDigest,
    argv: &'a [String],
    working_directory: &'a AbsPath,
    environment: &'a [(OsString, OsString)],
    timeout: Option<Duration>,
    network_access: &'a str,
    digest_config: CasDigestConfig,
}

/// The canonical key record and its digest.
#[derive(Clone)]
pub(crate) struct LocalCaseKey {
    record: Vec<u8>,
    digest: String,
    action_digest: String,
}

impl LocalCaseKey {
    fn new(material: KeyMaterial<'_>) -> Self {
        let mut record = KeyRecord::default();
        record.field("schema", LOCAL_CASE_RESULT_SCHEMA.as_bytes());
        record.field("engine_sha256", material.engine.as_bytes());
        record.field(
            "digest_algorithm",
            format!("{:?}", material.digest_config.preferred_algorithm()).as_bytes(),
        );
        record.field("target", material.case.target.as_bytes());
        record.field("suite", material.case.suite.as_bytes());
        record.field("case", material.case.case.as_bytes());
        record.field("discovery_sha256", material.case.discovery.as_bytes());
        record.field("action_digest", material.action_digest.to_string().as_bytes());
        record.count("argv", material.argv.len());
        for arg in material.argv {
            record.field("arg", arg.as_bytes());
        }
        record.field("cwd", os_bytes(material.working_directory.as_os_str()));
        record.count("env", material.environment.len());
        for (name, value) in material.environment {
            record.field("env_name", os_bytes(name));
            record.field("env_value", os_bytes(value));
        }
        record.field("stdin", b"null");
        record.field(
            "timeout_ms",
            match material.timeout {
                Some(timeout) => timeout.as_millis().to_string(),
                None => "none".to_owned(),
            }
            .as_bytes(),
        );
        record.field("network_access", material.network_access.as_bytes());
        record.field("executor", b"local-required");
        record.field("euid", effective_user().as_bytes());
        record.field("os", std::env::consts::OS.as_bytes());
        record.field("arch", std::env::consts::ARCH.as_bytes());
        let record = record.0;
        let digest = sha256(&record);
        Self {
            record,
            digest,
            action_digest: material.action_digest.to_string(),
        }
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    pub(crate) fn action_digest(&self) -> &str {
        &self.action_digest
    }

    fn content_digest(&self, bytes: &[u8]) -> String {
        sha256(bytes)
    }
}

/// Length-delimited `(name, value)` fields; no field boundary can be forged by another field's
/// bytes.
#[derive(Default)]
pub(crate) struct KeyRecord(pub(crate) Vec<u8>);

impl KeyRecord {
    pub(crate) fn field(&mut self, name: &str, value: &[u8]) {
        self.0.extend((name.len() as u64).to_be_bytes());
        self.0.extend(name.as_bytes());
        self.0.extend((value.len() as u64).to_be_bytes());
        self.0.extend(value);
    }

    pub(crate) fn count(&mut self, name: &str, count: usize) {
        self.field(name, &(count as u64).to_be_bytes());
    }
}

#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> &[u8] {
    value.as_encoded_bytes()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn effective_user() -> String {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }.to_string()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn effective_user() -> String {
    "unsupported".to_owned()
}

/// A pass confirmed by the in-process parser for exactly the expected case.
///
/// Only `ConfirmedPass::exact` constructs one, so publication cannot be reached from a generic
/// report, an empty or fallback parse, several results, a different name or a non-PASS status.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConfirmedPass {
    case_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NotConfirmed {
    NonZeroExit,
    NotExactlyOneResult(usize),
    DifferentCase,
    NotPass,
}

impl fmt::Display for NotConfirmed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonZeroExit => write!(f, "the case exited with a non-zero status"),
            Self::NotExactlyOneResult(n) => write!(f, "the parser returned {n} results, not 1"),
            Self::DifferentCase => write!(f, "the parsed result names a different case"),
            Self::NotPass => write!(f, "the parsed result is not PASS"),
        }
    }
}

impl ConfirmedPass {
    pub(crate) fn exact(
        expected_case: &str,
        exit_code: i32,
        results: &[(&str, &TestStatus)],
    ) -> Result<Self, NotConfirmed> {
        if exit_code != 0 {
            return Err(NotConfirmed::NonZeroExit);
        }
        let [(name, status)] = results else {
            return Err(NotConfirmed::NotExactlyOneResult(results.len()));
        };
        if *name != expected_case {
            return Err(NotConfirmed::DifferentCase);
        }
        if **status != TestStatus::PASS {
            return Err(NotConfirmed::NotPass);
        }
        Ok(Self {
            case_name: expected_case.to_owned(),
        })
    }
}

/// Provenance of a reused entry, as reported in place of an executed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReuseProvenance {
    pub(crate) action_digest: String,
    pub(crate) cache_key: String,
    pub(crate) origin_attempt: String,
    pub(crate) entry_manifest: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GenerationState {
    Pending,
    Succeeded,
    Unsuccessful,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct KeyState {
    schema: String,
    latest_generation: u64,
    latest_state: GenerationState,
    /// Why the latest generation is unsuccessful, when it is.
    latest_reason: Option<String>,
    /// The newest committed success, which is reusable only while it is also the latest
    /// generation.
    success_generation: Option<u64>,
}

impl KeyState {
    fn initial() -> Self {
        Self {
            schema: LOCAL_CASE_RESULT_SCHEMA.to_owned(),
            latest_generation: 0,
            latest_state: GenerationState::Unsuccessful,
            latest_reason: Some("no attempt".to_owned()),
            success_generation: None,
        }
    }

    fn reusable_generation(&self) -> Option<u64> {
        match (self.latest_state, self.success_generation) {
            (GenerationState::Succeeded, Some(generation))
                if generation == self.latest_generation =>
            {
                Some(generation)
            }
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct StreamRecord {
    len: u64,
    digest: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct EntryManifest {
    schema: String,
    key_digest: String,
    generation: u64,
    origin_attempt: String,
    action_digest: String,
    outcome: GenerationState,
    /// Why an unsuccessful generation did not publish.
    reason: Option<String>,
    /// The executor's terminal status, as reported.
    status: String,
    exit_code: Option<i32>,
    stdout: StreamRecord,
    stderr: StreamRecord,
    start_time_unix_ms: u128,
    execution_time_ms: u128,
    confirmed_case: Option<String>,
    confirmed_status: Option<String>,
}

/// An execution's exact raw streams and terminal facts, as the executor returned them.
pub(crate) struct ExecutionRecord {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) status: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) start_time: SystemTime,
    pub(crate) execution_time: Duration,
}

pub(crate) use store::CapturedCase;
pub(crate) use store::LocalCaseStore;
#[cfg(test)]
pub(crate) use store::Lease;
pub(crate) use store::Lookup;
pub(crate) use store::PendingCase;
pub(crate) use store::ReusedCase;

/// What the orchestrator hands the internal runner for one case: nothing, a reused entry that the
/// runner must still confirm through its parser, or a captured success awaiting confirmation.
///
/// The value is shared by clones of the execution data, but its content can be taken only once.
#[derive(Clone, Default, allocative::Allocative)]
pub(crate) struct LocalCaseOutcome(
    #[allocative(skip)] Option<std::sync::Arc<std::sync::Mutex<Option<LocalCaseResult>>>>,
);

pub(crate) enum LocalCaseResult {
    Reused(ReusedCase),
    Captured(CapturedCase),
    /// A tracked fresh generation already recorded as unsuccessful. It holds nothing, but tells
    /// the runner to report the case strictly.
    Unsuccessful,
}

impl LocalCaseOutcome {
    pub(crate) fn new(result: LocalCaseResult) -> Self {
        Self(Some(std::sync::Arc::new(std::sync::Mutex::new(Some(
            result,
        )))))
    }

    pub(crate) fn take(&self) -> Option<LocalCaseResult> {
        self.0
            .as_ref()
            .and_then(|cell| cell.lock().unwrap_or_else(|e| e.into_inner()).take())
    }
}

impl fmt::Debug for LocalCaseOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalCaseOutcome")
    }
}

/// Run one short filesystem step of the store on the blocking pool. No step waits for another
/// holder of a lease, so every step finishes without depending on other blocking work.
async fn blocking<T: Send + 'static>(
    step: impl FnOnce() -> buck2_error::Result<T> + Send + 'static,
) -> buck2_error::Result<T> {
    tokio::task::spawn_blocking(step).await.map_err(|e| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Environment,
            "Local case result store step did not complete: {e}"
        )
    })?
}

/// The first step of a constrained case, taken under its key's lease.
pub(crate) enum LocalCaseStart {
    Hit(ReusedCase),
    Begun(PendingCase),
}

impl LocalCaseStart {
    /// Open the store, wait for the key's lease and either return a verified hit or durably
    /// begin the next generation; the lease then stays with the returned hit or pending
    /// attempt. If this future is dropped first, any step in flight still finishes and its
    /// result is dropped, which releases the lease and records any begun generation as
    /// abandoned.
    pub(crate) async fn take(
        root: &str,
        key: &LocalCaseKey,
        reuse: bool,
        attempt: &str,
    ) -> buck2_error::Result<Self> {
        let root = root.to_owned();
        let store = blocking(move || LocalCaseStore::open(&root)).await?;
        let guard = store.lease(key).await?;
        let attempt = attempt.to_owned();
        blocking(move || {
            let guard = if reuse {
                match guard.lookup()? {
                    Lookup::Hit(reused) => return Ok(Self::Hit(reused)),
                    Lookup::Miss(guard) => guard,
                }
            } else {
                guard
            };
            Ok(Self::Begun(guard.begin(&attempt)?))
        })
        .await
    }
}

/// Settle a begun generation from its executor result: keep a local exit-0 success for
/// confirmation by the runner, or record the generation as unsuccessful with whatever exact raw
/// streams the executor returned, including those of a failed, timed-out or cancelled child.
pub(crate) async fn settle_execution(
    pending: PendingCase,
    report: &CommandExecutionReport,
) -> LocalCaseOutcome {
    let record = match &report.std_streams {
        CommandStdStreams::Local { stdout, stderr } => Some(ExecutionRecord {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            status: report.status.to_string(),
            exit_code: report.exit_code,
            start_time: report.timing.start_time,
            execution_time: report.timing.execution_time,
        }),
        // Streams that are not local bytes are not available here.
        _ => None,
    };
    match (&report.status, record) {
        (
            CommandExecutionStatus::Success {
                execution_kind: CommandExecutionKind::Local { .. },
            },
            Some(record),
        ) if report.exit_code == Some(0) => {
            LocalCaseOutcome::new(LocalCaseResult::Captured(pending.capture(record)))
        }
        (status, record) => {
            let reason = format!("execution did not succeed locally: {status}");
            if let Err(e) = blocking(move || pending.reject(&reason, record.as_ref())).await {
                tracing::warn!("Recording unsuccessful local case failed: {e:#}");
            }
            LocalCaseOutcome::new(LocalCaseResult::Unsuccessful)
        }
    }
}

/// Hide a reused entry the current parser did not confirm behind a new unsuccessful generation.
pub(crate) async fn reject_reused(reused: ReusedCase, reason: String) {
    if let Err(e) = blocking(move || reused.reject(&reason)).await {
        tracing::warn!("Invalidating an unconfirmed local case result failed: {e:#}");
    }
}

/// Publish a confirmed captured case after its result was reported, or record why not.
pub(crate) async fn settle_fresh(
    local_case: Option<LocalCaseResult>,
    confirmation: Result<ConfirmedPass, NotConfirmed>,
) {
    match (local_case, confirmation) {
        (Some(LocalCaseResult::Captured(captured)), Ok(confirmed)) => {
            if let Err(e) = blocking(move || captured.confirm(&confirmed)).await {
                tracing::warn!("Publishing confirmed local case result failed: {e:#}");
            }
        }
        (local_case, Err(reason)) => settle_unconfirmed(local_case, &reason.to_string()).await,
        (None | Some(LocalCaseResult::Unsuccessful), Ok(_)) => {}
        (Some(LocalCaseResult::Reused(reused)), Ok(_)) => drop(reused),
    }
}

/// Record a captured or reused case that was not confirmed. A captured generation becomes
/// unsuccessful; a reused entry is hidden.
pub(crate) async fn settle_unconfirmed(local_case: Option<LocalCaseResult>, reason: &str) {
    let reason = reason.to_owned();
    let settled = match local_case {
        None | Some(LocalCaseResult::Unsuccessful) => return,
        Some(LocalCaseResult::Captured(captured)) => {
            blocking(move || captured.reject(&reason)).await
        }
        Some(LocalCaseResult::Reused(reused)) => blocking(move || reused.reject(&reason)).await,
    };
    if let Err(e) = settled {
        tracing::warn!("Recording an unconfirmed local case failed: {e:#}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod store {
    use std::ffi::CStr;
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::time::UNIX_EPOCH;

    use buck2_error::BuckErrorContext;
    use buck2_error::BuckErrorOptionContext;
    use buck2_error::buck2_error;
    use dupe::Dupe;

    use super::*;

    const LAYOUT: &str = "v1";

    fn store_error(message: String) -> buck2_error::Error {
        buck2_error!(buck2_error::ErrorTag::Environment, "{}", message)
    }

    fn io_error(context: &str, name: &str) -> buck2_error::Error {
        let e = std::io::Error::last_os_error();
        store_error(format!("{context} `{name}`: {e}"))
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// A single path component: never empty, `.`, `..`, or containing `/` or NUL.
    fn component(name: &str) -> buck2_error::Result<CString> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(store_error(format!(
                "`{name}` is not a single path component"
            )));
        }
        CString::new(name).map_err(|_| store_error(format!("`{name}` contains NUL")))
    }

    fn fstat(fd: &OwnedFd, name: &str) -> buck2_error::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fd` is open and `stat` is a valid out-pointer.
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io_error("Inspecting", name));
        }
        // SAFETY: fstat succeeded and initialised it.
        Ok(unsafe { stat.assume_init() })
    }

    /// Require an opened object to be of `kind`, owned by this user, inaccessible to others and,
    /// for regular files, not hard-linked elsewhere.
    fn require_private(fd: &OwnedFd, name: &str, kind: libc::mode_t) -> buck2_error::Result<()> {
        let stat = fstat(fd, name)?;
        let actual_kind = stat.st_mode & libc::S_IFMT;
        if actual_kind != kind {
            return Err(store_error(format!(
                "Local case result entry `{name}` has file type {actual_kind:o}, expected {kind:o}"
            )));
        }
        if stat.st_uid != euid() || stat.st_mode & 0o077 != 0 {
            return Err(store_error(format!(
                "Local case result entry `{name}` must be owned by uid {} and inaccessible to \
                 others (found uid {}, mode {:o})",
                euid(),
                stat.st_uid,
                stat.st_mode & 0o7777
            )));
        }
        if kind == libc::S_IFREG && stat.st_nlink != 1 {
            return Err(store_error(format!(
                "Local case result file `{name}` has {} links",
                stat.st_nlink
            )));
        }
        Ok(())
    }

    fn sync_fd(fd: &OwnedFd, name: &str) -> buck2_error::Result<()> {
        // SAFETY: `fd` is open.
        if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
            return Err(io_error("Syncing", name));
        }
        Ok(())
    }

    fn unique_suffix() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("{}-{}", std::process::id(), nanos)
    }

    /// A validated private directory. Every access below it is relative to this descriptor
    /// with `O_NOFOLLOW`, so no path is re-resolved through a substituted ancestor or symlink.
    struct Dir {
        fd: OwnedFd,
        name: String,
    }

    /// Flags for an ancestor of the root, which is only searched. `O_PATH` needs no read
    /// permission, so an ancestor such as a mode 0711 home directory can still be walked.
    #[cfg(target_os = "linux")]
    const ANCESTOR_FLAGS: libc::c_int =
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    const ANCESTOR_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    /// The components of a configured root, which must be an absolute, lexically canonical path
    /// below `/`: no trailing slash and no empty, `.` or `..` component.
    fn root_components(path: &str) -> buck2_error::Result<Vec<CString>> {
        let invalid = |why: &str| {
            store_error(format!(
                "[test] local_case_result_cache_root `{path}` {why}; it must be an absolute, \
                 lexically canonical directory path"
            ))
        };
        let Some(relative) = path.strip_prefix('/') else {
            return Err(invalid("is not absolute"));
        };
        if relative.is_empty() {
            return Err(invalid("is the filesystem root"));
        }
        relative
            .split('/')
            .map(|c| match c {
                "" => Err(invalid("has an empty component or a trailing slash")),
                "." | ".." => Err(invalid("has a `.` or `..` component")),
                c => CString::new(c).map_err(|_| invalid("contains NUL")),
            })
            .collect()
    }

    /// An ancestor of the root must be a directory that no other principal can rename entries
    /// in: owned by root or this user and writable by neither group nor others, unless it is a
    /// root-owned sticky directory such as `/tmp`.
    fn require_trusted_ancestor(fd: &OwnedFd, name: &str) -> buck2_error::Result<()> {
        let stat = fstat(fd, name)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(store_error(format!(
                "Local case result root ancestor `{name}` is not a directory"
            )));
        }
        let owner_trusted = stat.st_uid == 0 || stat.st_uid == euid();
        let shared_write = stat.st_mode & 0o022 != 0;
        let root_sticky = stat.st_uid == 0 && stat.st_mode & libc::S_ISVTX != 0;
        if !owner_trusted || (shared_write && !root_sticky) {
            return Err(store_error(format!(
                "Local case result root ancestor `{name}` may be changed by another user \
                 (uid {}, mode {:o}); every ancestor must be owned by root or uid {} and not \
                 writable by others unless it is a root-owned sticky directory",
                stat.st_uid,
                stat.st_mode & 0o7777,
                euid()
            )));
        }
        Ok(())
    }

    impl Dir {
        /// Open (creating the last component if absent) the configured root by walking it from
        /// `/` one component at a time with `O_NOFOLLOW`. A symlink anywhere in the path, an
        /// ancestor another principal could change, or a root that is not private is refused.
        fn open_root(path: &str) -> buck2_error::Result<Self> {
            let components = root_components(path)?;
            let (last, ancestors) = components
                .split_last()
                .internal_error("root has components")?;
            // SAFETY: valid C string; the returned descriptor is owned below.
            let fd = unsafe { libc::open(c"/".as_ptr(), ANCESTOR_FLAGS) };
            if fd < 0 {
                return Err(io_error("Opening", "/"));
            }
            // SAFETY: freshly opened descriptor.
            let mut parent = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut walked = String::new();
            require_trusted_ancestor(&parent, "/")?;
            for component in ancestors {
                walked.push('/');
                walked.push_str(&component.to_string_lossy());
                // SAFETY: `parent` is an open directory and `component` a valid component.
                let fd =
                    unsafe { libc::openat(parent.as_raw_fd(), component.as_ptr(), ANCESTOR_FLAGS) };
                if fd < 0 {
                    return Err(io_error(
                        "Opening (symlinks are refused) root ancestor",
                        &walked,
                    ));
                }
                // SAFETY: freshly opened descriptor.
                parent = unsafe { OwnedFd::from_raw_fd(fd) };
                require_trusted_ancestor(&parent, &walked)?;
            }
            // SAFETY: `parent` is an open directory and `last` a valid component.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), last.as_ptr(), 0o700) } != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(io_error("Creating", path));
            }
            // SAFETY: as above.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    last.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io_error("Opening (symlinks are refused)", path));
            }
            // SAFETY: freshly opened descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            require_private(&fd, path, libc::S_IFDIR)?;
            Ok(Self {
                fd,
                name: path.to_owned(),
            })
        }

        fn child_name(&self, name: &str) -> String {
            format!("{}/{}", self.name, name)
        }

        fn openat(&self, name: &CStr, flags: libc::c_int, mode: libc::c_uint) -> libc::c_int {
            // SAFETY: `self.fd` is an open directory and `name` a valid component.
            unsafe { libc::openat(self.fd.as_raw_fd(), name.as_ptr(), flags, mode) }
        }

        /// Open (creating if absent) the private child directory `name`.
        fn dir(&self, name: &str) -> buck2_error::Result<Dir> {
            let c_name = component(name)?;
            let full = self.child_name(name);
            // SAFETY: as in `openat`.
            if unsafe { libc::mkdirat(self.fd.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(io_error("Creating", &full));
            }
            let fd = self.openat(
                &c_name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            );
            if fd < 0 {
                return Err(io_error("Opening", &full));
            }
            // SAFETY: freshly opened descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            require_private(&fd, &full, libc::S_IFDIR)?;
            Ok(Dir { fd, name: full })
        }

        /// Open an existing private child directory; absence is an error.
        fn existing_dir(&self, name: &str) -> buck2_error::Result<Dir> {
            let c_name = component(name)?;
            let full = self.child_name(name);
            let fd = self.openat(
                &c_name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            );
            if fd < 0 {
                return Err(io_error("Opening", &full));
            }
            // SAFETY: freshly opened descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            require_private(&fd, &full, libc::S_IFDIR)?;
            Ok(Dir { fd, name: full })
        }

        /// Read a private regular file. `O_NONBLOCK` keeps a substituted FIFO from blocking the
        /// open; the type check then refuses it.
        fn read(&self, name: &str) -> buck2_error::Result<Option<Vec<u8>>> {
            let c_name = component(name)?;
            let full = self.child_name(name);
            let fd = self.openat(
                &c_name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            );
            if fd < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(io_error("Opening", &full));
            }
            // SAFETY: freshly opened descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            require_private(&fd, &full, libc::S_IFREG)?;
            let mut bytes = Vec::new();
            File::from(fd)
                .read_to_end(&mut bytes)
                .with_buck_error_context(|| format!("Reading `{full}`"))?;
            Ok(Some(bytes))
        }

        fn read_required(&self, name: &str) -> buck2_error::Result<Vec<u8>> {
            self.read(name)?.ok_or_else(|| {
                store_error(format!("Local case result file `{}` is missing", self.child_name(name)))
            })
        }

        /// Create `name`, which must not exist, with `bytes`, and sync it.
        fn write_new(&self, name: &str, bytes: &[u8]) -> buck2_error::Result<()> {
            let c_name = component(name)?;
            let full = self.child_name(name);
            let fd = self.openat(
                &c_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            );
            if fd < 0 {
                return Err(io_error("Creating", &full));
            }
            // SAFETY: freshly opened descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            require_private(&fd, &full, libc::S_IFREG)?;
            let mut file = File::from(fd);
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .with_buck_error_context(|| format!("Writing `{full}`"))
        }

        /// Rename the child `from` of this directory to `to` in `target`, then sync both.
        fn rename(&self, from: &str, target: &Dir, to: &str) -> buck2_error::Result<()> {
            let c_from = component(from)?;
            let c_to = component(to)?;
            // SAFETY: both directory descriptors are open and the names are valid components.
            if unsafe {
                libc::renameat(
                    self.fd.as_raw_fd(),
                    c_from.as_ptr(),
                    target.fd.as_raw_fd(),
                    c_to.as_ptr(),
                )
            } != 0
            {
                return Err(io_error("Publishing", &target.child_name(to)));
            }
            sync_fd(&target.fd, &target.name)?;
            if !std::ptr::eq(self, target) {
                sync_fd(&self.fd, &self.name)?;
            }
            Ok(())
        }

        /// Whether `name` exists, without following a final symlink.
        fn exists(&self, name: &str) -> buck2_error::Result<bool> {
            let c_name = component(name)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: valid descriptor, component and out-pointer.
            let rc = unsafe {
                libc::fstatat(
                    self.fd.as_raw_fd(),
                    c_name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc == 0 {
                return Ok(true);
            }
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(false);
            }
            Err(io_error("Inspecting", &self.child_name(name)))
        }

        /// Atomically replace `name` with `bytes`.
        fn replace(&self, name: &str, bytes: &[u8]) -> buck2_error::Result<()> {
            let staged = format!(".{name}.{}", unique_suffix());
            self.write_new(&staged, bytes)?;
            self.rename(&staged, self, name)
        }
    }

    /// The root directory a configured spelling was first found to name, and its layout
    /// directory, both held open for the life of the process. Every key, lock and generation
    /// operation is relative to these descriptors; the configured path is never resolved again
    /// to reach them.
    struct PinnedRoot {
        layout: Dir,
        device: u64,
        inode: u64,
    }

    impl fmt::Debug for PinnedRoot {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "PinnedRoot({})", self.layout.name)
        }
    }

    /// Roots pinned by this process, by configured spelling.
    static PINNED_ROOTS: std::sync::Mutex<Vec<(String, std::sync::Arc<PinnedRoot>)>> =
        std::sync::Mutex::new(Vec::new());

    /// A validated private store root, pinned by descriptor.
    #[derive(Clone, Debug)]
    pub(crate) struct LocalCaseStore {
        root: std::sync::Arc<PinnedRoot>,
    }

    /// Whether the key's exclusive lease could be taken.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) enum Lease {
        Held(KeyGuard),
        /// Another attempt owns the key; this one executes without reuse or publication.
        Busy,
    }

    /// The result of a lookup under a held lease.
    pub(crate) enum Lookup {
        Hit(ReusedCase),
        /// No reusable generation; the lease is returned for a new attempt.
        Miss(KeyGuard),
    }

    impl LocalCaseStore {
        /// Open the store under an explicitly configured root. This blocks on the filesystem.
        ///
        /// The root must be an absolute, lexically canonical path every component of which is a
        /// real directory, with no ancestor another principal could change, and the root itself
        /// private to this user (see `Dir::open_root`). The directory found the first time a
        /// spelling is opened is pinned for the life of the process: if the spelling later
        /// names a different directory, the store is refused rather than silently switching
        /// key, lock and generation namespaces, until the daemon restarts.
        pub(crate) fn open(path: &str) -> buck2_error::Result<Self> {
            let root = Dir::open_root(path)?;
            let stat = fstat(&root.fd, path)?;
            let (device, inode) = (stat.st_dev as u64, stat.st_ino as u64);
            let mut pinned = PINNED_ROOTS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, existing)) = pinned.iter().find(|(spelling, _)| spelling == path) {
                if (existing.device, existing.inode) != (device, inode) {
                    return Err(store_error(format!(
                        "[test] local_case_result_cache_root `{path}` now names a different \
                         directory than the one this daemon pinned; restart the daemon to use \
                         the new directory"
                    )));
                }
                return Ok(Self {
                    root: existing.dupe(),
                });
            }
            let root = std::sync::Arc::new(PinnedRoot {
                layout: root.dir(LAYOUT)?,
                device,
                inode,
            });
            pinned.push((path.to_owned(), root.dupe()));
            Ok(Self { root })
        }

        /// Take the exclusive per-key lease, waiting for another holder to finish. It spans the
        /// whole use of the key: a hit until the runner confirms or rejects it, or an attempt
        /// from its pending record through capture and publication. Same-key attempts therefore
        /// never overlap each other or a hit.
        ///
        /// A waiter holds no blocking thread while the lease is held elsewhere: each attempt to
        /// take it is a short non-blocking step, and the wait between attempts is an async
        /// timer. The holder's settlement, which runs on the blocking pool, therefore never
        /// queues behind waiters for its own lease. Dropping the returned future abandons the
        /// wait; a lease taken by an attempt still in flight is released when its result is
        /// dropped.
        pub(crate) async fn lease(&self, key: &LocalCaseKey) -> buck2_error::Result<KeyGuard> {
            const FIRST_RETRY: Duration = Duration::from_millis(1);
            const LONGEST_RETRY: Duration = Duration::from_millis(50);
            let mut retry = FIRST_RETRY;
            loop {
                let store = self.clone();
                let attempt_key = key.clone();
                match blocking(move || store.try_lease(&attempt_key)).await? {
                    Lease::Held(guard) => return Ok(guard),
                    Lease::Busy => {
                        tokio::time::sleep(retry).await;
                        retry = (retry * 2).min(LONGEST_RETRY);
                    }
                }
            }
        }

        /// Take the lease only if no one holds it, without waiting. This blocks only on the
        /// filesystem.
        pub(crate) fn try_lease(&self, key: &LocalCaseKey) -> buck2_error::Result<Lease> {
            let dir = self.root.layout.dir(&key.digest)?;
            let lock_name = component("lock")?;
            let fd = dir.openat(
                &lock_name,
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600,
            );
            if fd < 0 {
                return Err(io_error("Opening", &dir.child_name("lock")));
            }
            // SAFETY: freshly opened descriptor.
            let lock = unsafe { OwnedFd::from_raw_fd(fd) };
            require_private(&lock, &dir.child_name("lock"), libc::S_IFREG)?;
            // Never `LOCK_EX` without `LOCK_NB`: a blocking wait would occupy a blocking-pool
            // thread that the holder may need to settle and release this very lease.
            // SAFETY: `lock` is open for the lifetime of the guard.
            while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    return Ok(Lease::Busy);
                }
                return Err(store_error(format!(
                    "Locking `{}`: {e}",
                    dir.child_name("lock")
                )));
            }
            let generations = dir.dir("generations")?;
            match dir.read("key")? {
                Some(existing) if existing == key.record => {}
                Some(_) => {
                    return Err(store_error(format!(
                        "Local case key record `{}` does not match its digest",
                        dir.child_name("key")
                    )));
                }
                None => dir.replace("key", &key.record)?,
            }
            Ok(Lease::Held(KeyGuard {
                key: key.clone(),
                dir,
                generations,
                _lock: lock,
                _root: self.root.dupe(),
            }))
        }
    }

    /// The exclusive lease on one key, released when the last owner drops it.
    pub(crate) struct KeyGuard {
        key: LocalCaseKey,
        dir: Dir,
        generations: Dir,
        _lock: OwnedFd,
        _root: std::sync::Arc<PinnedRoot>,
    }

    impl KeyGuard {
        fn read_state(&self) -> buck2_error::Result<KeyState> {
            match self.dir.read("state.json")? {
                None => Ok(KeyState::initial()),
                Some(bytes) => {
                    let state: KeyState = serde_json::from_slice(&bytes)
                        .with_buck_error_context(|| {
                            format!("Parsing `{}`", self.dir.child_name("state.json"))
                        })?;
                    if state.schema != LOCAL_CASE_RESULT_SCHEMA {
                        return Err(store_error(format!(
                            "`{}` has schema `{}`",
                            self.dir.child_name("state.json"),
                            state.schema
                        )));
                    }
                    Ok(state)
                }
            }
        }

        fn write_state(&self, state: &KeyState) -> buck2_error::Result<()> {
            let bytes = serde_json::to_vec_pretty(state)
                .buck_error_context("Serializing local case state")?;
            self.dir.replace("state.json", &bytes)
        }

        /// Return the reusable entry, verified against its manifest, or the lease for a new
        /// attempt. Corrupt or mismatched entries are errors, never hits, and are left in place.
        pub(crate) fn lookup(self) -> buck2_error::Result<Lookup> {
            let state = self.read_state()?;
            let Some(generation) = state.reusable_generation() else {
                return Ok(Lookup::Miss(self));
            };
            let entry = self.generations.existing_dir(&generation.to_string())?;
            let manifest_bytes = entry.read_required("manifest.json")?;
            let manifest: EntryManifest = serde_json::from_slice(&manifest_bytes)
                .buck_error_context("Parsing local case manifest")?;
            let key = &self.key;
            if manifest.schema != LOCAL_CASE_RESULT_SCHEMA
                || manifest.key_digest != key.digest
                || manifest.generation != generation
                || manifest.action_digest != key.action_digest
                || manifest.outcome != GenerationState::Succeeded
                || manifest.exit_code != Some(0)
                || manifest.confirmed_status.as_deref() != Some("PASS")
            {
                return Err(store_error(format!(
                    "Local case manifest in `{}` does not describe a confirmed pass for this key",
                    entry.name
                )));
            }
            let stdout = entry.read_required("stdout")?;
            let stderr = entry.read_required("stderr")?;
            for (name, bytes, record) in [
                ("stdout", &stdout, &manifest.stdout),
                ("stderr", &stderr, &manifest.stderr),
            ] {
                if bytes.len() as u64 != record.len || key.content_digest(bytes) != record.digest
                {
                    return Err(store_error(format!(
                        "Local case {name} in `{}` does not match its manifest",
                        entry.name
                    )));
                }
            }
            let provenance = ReuseProvenance {
                action_digest: key.action_digest.clone(),
                cache_key: key.digest.clone(),
                origin_attempt: manifest.origin_attempt,
                entry_manifest: key.content_digest(&manifest_bytes),
            };
            Ok(Lookup::Hit(ReusedCase {
                guard: self,
                generation,
                provenance,
                stdout,
                stderr,
            }))
        }

        /// Durably start a new generation before its child is dispatched.
        pub(crate) fn begin(self, attempt: &str) -> buck2_error::Result<PendingCase> {
            let mut state = self.read_state()?;
            let generation = state
                .latest_generation
                .checked_add(1)
                .internal_error("local case generation overflow")?;
            state.latest_generation = generation;
            state.latest_state = GenerationState::Pending;
            state.latest_reason = None;
            self.write_state(&state)?;
            Ok(PendingCase {
                attempt: Some(Attempt {
                    guard: self,
                    generation,
                    origin: attempt.to_owned(),
                    scratch: None,
                }),
            })
        }

        /// Record `generation` as unsuccessful if it is still the latest pending one, keeping the
        /// execution's exact raw streams when there are any. The state is written even if the
        /// streams cannot be.
        fn settle_unsuccessful(
            &self,
            generation: u64,
            origin: &str,
            reason: &str,
            record: Option<&ExecutionRecord>,
        ) -> buck2_error::Result<()> {
            let mut state = self.read_state()?;
            if state.latest_generation != generation
                || state.latest_state != GenerationState::Pending
            {
                return Ok(());
            }
            let kept = match record {
                Some(record) => self
                    .write_generation(
                        generation,
                        origin,
                        GenerationState::Unsuccessful,
                        Some(reason),
                        record,
                        None,
                    )
                    .map(|_| ()),
                None => Ok(()),
            };
            state.latest_state = GenerationState::Unsuccessful;
            state.latest_reason = Some(reason.to_owned());
            self.write_state(&state)?;
            kept
        }

        /// Commit `generations/<generation>` with the record's exact raw streams and manifest.
        /// An existing generation directory is never replaced.
        fn write_generation(
            &self,
            generation: u64,
            origin: &str,
            outcome: GenerationState,
            reason: Option<&str>,
            record: &ExecutionRecord,
            confirmed: Option<&ConfirmedPass>,
        ) -> buck2_error::Result<Vec<u8>> {
            let key = &self.key;
            let manifest = EntryManifest {
                schema: LOCAL_CASE_RESULT_SCHEMA.to_owned(),
                key_digest: key.digest.clone(),
                generation,
                origin_attempt: origin.to_owned(),
                action_digest: key.action_digest.clone(),
                outcome,
                reason: reason.map(str::to_owned),
                status: record.status.clone(),
                exit_code: record.exit_code,
                stdout: StreamRecord {
                    len: record.stdout.len() as u64,
                    digest: key.content_digest(&record.stdout),
                },
                stderr: StreamRecord {
                    len: record.stderr.len() as u64,
                    digest: key.content_digest(&record.stderr),
                },
                start_time_unix_ms: record
                    .start_time
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis()),
                execution_time_ms: record.execution_time.as_millis(),
                confirmed_case: confirmed.map(|c| c.case_name.clone()),
                confirmed_status: confirmed.map(|_| "PASS".to_owned()),
            };
            let manifest_bytes = serde_json::to_vec_pretty(&manifest)
                .buck_error_context("Serializing local case manifest")?;
            let committed = generation.to_string();
            if self.generations.exists(&committed)? {
                return Err(store_error(format!(
                    "Local case generation `{}` already exists",
                    self.generations.child_name(&committed)
                )));
            }
            let staged_name = format!(".staging-{}", unique_suffix());
            let staged = self.generations.dir(&staged_name)?;
            staged.write_new("stdout", &record.stdout)?;
            staged.write_new("stderr", &record.stderr)?;
            staged.write_new("manifest.json", &manifest_bytes)?;
            sync_fd(&staged.fd, &staged.name)?;
            self.generations
                .rename(&staged_name, &self.generations, &committed)?;
            Ok(manifest_bytes)
        }

        fn publish(
            &self,
            generation: u64,
            origin: &str,
            original: &ExecutionRecord,
            confirmed: &ConfirmedPass,
        ) -> buck2_error::Result<ReuseProvenance> {
            let key = &self.key;
            let mut state = self.read_state()?;
            if state.latest_generation != generation
                || state.latest_state != GenerationState::Pending
            {
                return Err(store_error(format!(
                    "Local case generation {generation} is no longer the pending attempt \
                     (latest {} is {:?})",
                    state.latest_generation, state.latest_state
                )));
            }
            if original.exit_code != Some(0) {
                return Err(store_error(format!(
                    "Local case generation {generation} did not exit 0"
                )));
            }
            let manifest_bytes = self.write_generation(
                generation,
                origin,
                GenerationState::Succeeded,
                None,
                original,
                Some(confirmed),
            )?;
            state.latest_state = GenerationState::Succeeded;
            state.success_generation = Some(generation);
            self.write_state(&state)?;
            Ok(ReuseProvenance {
                action_digest: key.action_digest.clone(),
                cache_key: key.digest.clone(),
                origin_attempt: origin.to_owned(),
                entry_manifest: key.content_digest(&manifest_bytes),
            })
        }
    }

    struct Attempt {
        guard: KeyGuard,
        generation: u64,
        origin: String,
        /// The fresh-directory scratch path the attempt's execution uses, held as long as the
        /// key's lease: until the generation is published or recorded unsuccessful.
        scratch: Option<ScratchLease>,
    }

    /// A durable pending generation, holding the key's lease, whose child has not yet produced
    /// a capturable success.
    ///
    /// Dropping it records the generation as unsuccessful; the pending record already hides
    /// older successes even if that write fails.
    pub(crate) struct PendingCase {
        attempt: Option<Attempt>,
    }

    impl PendingCase {
        /// Hold `scratch` for as long as this attempt holds its key's lease.
        pub(crate) fn holding(mut self, scratch: ScratchLease) -> Self {
            if let Some(attempt) = self.attempt.as_mut() {
                attempt.scratch = Some(scratch);
            }
            self
        }

        /// Keep the exact raw streams of a local exit-0 execution until it is confirmed.
        pub(crate) fn capture(mut self, original: ExecutionRecord) -> CapturedCase {
            CapturedCase {
                pending: PendingCase {
                    attempt: self.attempt.take(),
                },
                original,
            }
        }

        /// Record the generation as unsuccessful, keeping the execution's raw streams if any.
        pub(crate) fn reject(
            mut self,
            reason: &str,
            record: Option<&ExecutionRecord>,
        ) -> buck2_error::Result<()> {
            match self.attempt.take() {
                Some(a) => a
                    .guard
                    .settle_unsuccessful(a.generation, &a.origin, reason, record),
                None => Ok(()),
            }
        }

        /// Release the lease without settling, as a process that died mid-attempt would.
        #[cfg(test)]
        pub(crate) fn simulate_crash(mut self) {
            drop(self.attempt.take().map(|a| a.guard));
        }

        #[cfg(test)]
        pub(crate) fn generation(&self) -> Option<u64> {
            self.attempt.as_ref().map(|a| a.generation)
        }
    }

    impl Drop for PendingCase {
        fn drop(&mut self) {
            if let Some(a) = self.attempt.take() {
                if let Err(e) = a.guard.settle_unsuccessful(
                    a.generation,
                    &a.origin,
                    "abandoned before semantic confirmation",
                    None,
                ) {
                    tracing::warn!("Recording abandoned local case generation failed: {e:#}");
                }
            }
        }
    }

    /// A local exit-0 execution awaiting exact semantic confirmation; one use.
    pub(crate) struct CapturedCase {
        pending: PendingCase,
        original: ExecutionRecord,
    }

    impl CapturedCase {
        #[cfg(test)]
        pub(crate) fn original(&self) -> &ExecutionRecord {
            &self.original
        }

        /// Commit the entry for this generation after the case was confirmed and reported.
        pub(crate) fn confirm(
            mut self,
            confirmed: &ConfirmedPass,
        ) -> buck2_error::Result<ReuseProvenance> {
            let attempt = self
                .pending
                .attempt
                .take()
                .internal_error("captured local case was already settled")?;
            match attempt
                .guard
                .publish(attempt.generation, &attempt.origin, &self.original, confirmed)
            {
                Ok(provenance) => Ok(provenance),
                Err(e) => {
                    let _ignored = attempt.guard.settle_unsuccessful(
                        attempt.generation,
                        &attempt.origin,
                        "publication failed",
                        Some(&self.original),
                    );
                    Err(e)
                }
            }
        }

        /// Record the generation as unsuccessful, keeping its exact raw streams.
        pub(crate) fn reject(mut self, reason: &str) -> buck2_error::Result<()> {
            match self.pending.attempt.take() {
                Some(a) => a.guard.settle_unsuccessful(
                    a.generation,
                    &a.origin,
                    reason,
                    Some(&self.original),
                ),
                None => Ok(()),
            }
        }
    }

    impl Drop for CapturedCase {
        /// A captured case dropped unconfirmed, for example when parsing or reporting failed,
        /// is unsuccessful and keeps its exact raw streams.
        fn drop(&mut self) {
            if let Some(a) = self.pending.attempt.take() {
                if let Err(e) = a.guard.settle_unsuccessful(
                    a.generation,
                    &a.origin,
                    "dropped before semantic confirmation",
                    Some(&self.original),
                ) {
                    tracing::warn!("Recording an unconfirmed local case failed: {e:#}");
                }
            }
        }
    }

    /// A verified reusable entry, holding the key's lease until the runner settles it. The
    /// runner must still confirm its raw streams through the current parser; `reject` hides it
    /// behind a new unsuccessful generation if that fails.
    pub(crate) struct ReusedCase {
        guard: KeyGuard,
        generation: u64,
        provenance: ReuseProvenance,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl ReusedCase {
        pub(crate) fn provenance(&self) -> &ReuseProvenance {
            &self.provenance
        }

        pub(crate) fn stdout(&self) -> &[u8] {
            &self.stdout
        }

        pub(crate) fn stderr(&self) -> &[u8] {
            &self.stderr
        }

        #[cfg(test)]
        pub(crate) fn generation(&self) -> u64 {
            self.generation
        }

        pub(crate) fn reject(self, reason: &str) -> buck2_error::Result<()> {
            let mut state = self.guard.read_state()?;
            if state.reusable_generation() == Some(self.generation) {
                state.latest_generation = self
                    .generation
                    .checked_add(1)
                    .internal_error("local case generation overflow")?;
                state.latest_state = GenerationState::Unsuccessful;
                state.latest_reason = Some(reason.to_owned());
                self.guard.write_state(&state)?;
            }
            Ok(())
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod store {
    use super::*;

    fn unsupported() -> buck2_error::Error {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Local case result reuse is supported only on Linux and macOS"
        )
    }

    pub(crate) enum Never {}

    #[derive(Clone, Debug)]
    pub(crate) struct LocalCaseStore;
    pub(crate) struct KeyGuard(Never);
    pub(crate) struct PendingCase(Never);
    pub(crate) struct CapturedCase(Never);
    pub(crate) struct ReusedCase(Never);

    pub(crate) enum Lease {
        Held(KeyGuard),
        Busy,
    }

    pub(crate) enum Lookup {
        Hit(ReusedCase),
        Miss(KeyGuard),
    }

    impl LocalCaseStore {
        pub(crate) fn open(_root: &str) -> buck2_error::Result<Self> {
            Err(unsupported())
        }
        pub(crate) async fn lease(&self, _key: &LocalCaseKey) -> buck2_error::Result<KeyGuard> {
            Err(unsupported())
        }
    }

    impl KeyGuard {
        pub(crate) fn lookup(self) -> buck2_error::Result<Lookup> {
            match self.0 {}
        }
        pub(crate) fn begin(self, _attempt: &str) -> buck2_error::Result<PendingCase> {
            match self.0 {}
        }
    }

    impl PendingCase {
        pub(crate) fn holding(self, _scratch: ScratchLease) -> Self {
            match self.0 {}
        }
        pub(crate) fn capture(self, _original: ExecutionRecord) -> CapturedCase {
            match self.0 {}
        }
        pub(crate) fn reject(
            self,
            _reason: &str,
            _record: Option<&ExecutionRecord>,
        ) -> buck2_error::Result<()> {
            match self.0 {}
        }
    }

    impl CapturedCase {
        pub(crate) fn original(&self) -> &ExecutionRecord {
            match self.0 {}
        }
        pub(crate) fn confirm(self, _c: &ConfirmedPass) -> buck2_error::Result<ReuseProvenance> {
            match self.0 {}
        }
        pub(crate) fn reject(self, _reason: &str) -> buck2_error::Result<()> {
            match self.0 {}
        }
    }

    impl ReusedCase {
        pub(crate) fn provenance(&self) -> &ReuseProvenance {
            match self.0 {}
        }
        pub(crate) fn stdout(&self) -> &[u8] {
            match self.0 {}
        }
        pub(crate) fn stderr(&self) -> &[u8] {
            match self.0 {}
        }
        pub(crate) fn generation(&self) -> u64 {
            match self.0 {}
        }
        pub(crate) fn reject(self, _reason: &str) -> buck2_error::Result<()> {
            match self.0 {}
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use buck2_common::cas_digest::CasDigestConfig;

    use super::*;

    /// A private, test-owned store root under the process temporary directory.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "buck2-local-case-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn store(&self) -> LocalCaseStore {
            LocalCaseStore::open(self.0.to_str().unwrap()).unwrap()
        }

        fn key_dir(&self, key: &LocalCaseKey) -> PathBuf {
            self.0.join("v1").join(key.digest())
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ignored = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Material {
        case: CaseIdentity,
        action: String,
        argv: Vec<String>,
        cwd: String,
        env: Vec<(OsString, OsString)>,
        timeout: Option<Duration>,
        network: String,
        engine: String,
    }

    impl Material {
        fn new() -> Self {
            Self {
                case: CaseIdentity {
                    target: "root//pkg:test (cfg)".to_owned(),
                    suite: "root//pkg:test".to_owned(),
                    case: "module::case".to_owned(),
                    discovery: "discovery".to_owned(),
                },
                action: "action".to_owned(),
                argv: vec!["runner".to_owned(), "module::case".to_owned()],
                cwd: "/project".to_owned(),
                env: vec![
                    (OsString::from("PATH"), OsString::from("/bin")),
                    (OsString::from("TMPDIR"), OsString::from("/tmp/a")),
                    (OsString::from("PWD"), OsString::from("/project")),
                ],
                timeout: Some(Duration::from_secs(60)),
                network: "None".to_owned(),
                engine: "engine".to_owned(),
            }
        }

        fn key(&self) -> LocalCaseKey {
            let config = CasDigestConfig::testing_default();
            let action = ActionDigest::from_content(self.action.as_bytes(), config);
            LocalCaseKey::new(KeyMaterial {
                engine: &self.engine,
                case: &self.case,
                action_digest: &action,
                argv: &self.argv,
                working_directory: AbsPath::new(&self.cwd).unwrap(),
                environment: &self.env,
                timeout: self.timeout,
                network_access: &self.network,
                digest_config: config,
            })
        }
    }

    fn original(stdout: &[u8], stderr: &[u8]) -> ExecutionRecord {
        ExecutionRecord {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            status: "success".to_owned(),
            exit_code: Some(0),
            start_time: SystemTime::now(),
            execution_time: Duration::from_millis(5),
        }
    }

    fn held(store: &LocalCaseStore, key: &LocalCaseKey) -> KeyGuardForTest {
        match store.try_lease(key).unwrap() {
            Lease::Held(guard) => KeyGuardForTest(guard),
            Lease::Busy => panic!("lease unexpectedly busy"),
        }
    }

    struct KeyGuardForTest(store::KeyGuard);

    enum Seen {
        Hit(ReusedCase),
        Miss(store::KeyGuard),
    }

    fn look(store: &LocalCaseStore, key: &LocalCaseKey) -> Seen {
        match held(store, key).0.lookup().unwrap() {
            Lookup::Hit(reused) => Seen::Hit(reused),
            Lookup::Miss(guard) => Seen::Miss(guard),
        }
    }

    fn pass(case: &str) -> ConfirmedPass {
        ConfirmedPass::exact(case, 0, &[(case, &TestStatus::PASS)]).unwrap()
    }

    /// Produce and confirm one generation, returning its origin attempt.
    fn produce(store: &LocalCaseStore, key: &LocalCaseKey, attempt: &str, stdout: &[u8]) {
        let Seen::Miss(guard) = look(store, key) else {
            panic!("expected a miss before producing");
        };
        let captured = guard
            .begin(attempt)
            .unwrap()
            .capture(original(stdout, b"stderr\xff"));
        captured.confirm(&pass("module::case")).unwrap();
    }

    #[test]
    fn confirmed_success_is_reused_with_exact_original_raw() {
        let root = Root::new("hit");
        let store = root.store();
        let key = Material::new().key();
        let raw = b"running 1 test\0\xfe\xff tail without newline";
        produce(&store, &key, "attempt-1", raw);

        let Seen::Hit(reused) = look(&store, &key) else {
            panic!("expected a hit");
        };
        assert_eq!(reused.stdout(), raw);
        assert_eq!(reused.stderr(), b"stderr\xff");
        assert_eq!(reused.provenance().origin_attempt, "attempt-1");
        assert_eq!(reused.provenance().cache_key, key.digest());
        assert_eq!(reused.generation(), 1);
    }

    #[test]
    fn unconfirmed_failed_or_abandoned_attempts_never_publish() {
        let root = Root::new("no-publish");
        let store = root.store();
        let key = Material::new().key();

        let Seen::Miss(guard) = look(&store, &key) else {
            panic!()
        };
        let failed = ExecutionRecord {
            stdout: b"before failure\0\xff".to_vec(),
            stderr: b"panicked\xfe without newline".to_vec(),
            status: "failure".to_owned(),
            exit_code: Some(101),
            start_time: SystemTime::now(),
            execution_time: Duration::from_millis(3),
        };
        guard
            .begin("rejected")
            .unwrap()
            .reject("exit 101", Some(&failed))
            .unwrap();
        assert!(matches!(look(&store, &key), Seen::Miss(_)));
        // The failed generation keeps its exact raw streams and terminal facts.
        let gen1 = root.key_dir(&key).join("generations/1");
        assert_eq!(std::fs::read(gen1.join("stdout")).unwrap(), failed.stdout);
        assert_eq!(std::fs::read(gen1.join("stderr")).unwrap(), failed.stderr);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(gen1.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["outcome"], "unsuccessful");
        assert_eq!(manifest["exit_code"], 101);
        assert_eq!(manifest["reason"], "exit 101");

        let Seen::Miss(guard) = look(&store, &key) else {
            panic!()
        };
        let captured = guard.begin("unconfirmed").unwrap().capture(original(b"out", b""));
        assert_eq!(captured.original().stdout, b"out");
        captured.reject("parser returned 0 results").unwrap();
        assert!(matches!(look(&store, &key), Seen::Miss(_)));
        // An unconfirmed exit-0 generation keeps its raw streams but is never a hit.
        assert_eq!(
            std::fs::read(root.key_dir(&key).join("generations/2/stdout")).unwrap(),
            b"out"
        );

        let Seen::Miss(guard) = look(&store, &key) else {
            panic!()
        };
        drop(guard.begin("abandoned").unwrap());
        assert!(matches!(look(&store, &key), Seen::Miss(_)));

        // A process that dies with the generation pending leaves it pending.
        let Seen::Miss(guard) = look(&store, &key) else {
            panic!()
        };
        let pending = guard.begin("crashed").unwrap();
        assert_eq!(pending.generation(), Some(4));
        pending.simulate_crash();
        assert!(matches!(look(&store, &key), Seen::Miss(_)));
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.key_dir(&key).join("state.json")).unwrap())
                .unwrap();
        assert_eq!(state["latest_generation"], 4);
        assert_eq!(state["latest_state"], "pending");
        assert_eq!(state["success_generation"], serde_json::Value::Null);
    }

    #[test]
    fn newer_unsuccessful_generations_never_resurrect_an_older_success() {
        let root = Root::new("resurrect");
        let store = root.store();
        let key = Material::new().key();
        produce(&store, &key, "first-pass", b"first");

        // The current parser no longer confirms the entry: it is hidden.
        let Seen::Hit(reused) = look(&store, &key) else {
            panic!()
        };
        reused.reject("not confirmed").unwrap();
        assert!(matches!(look(&store, &key), Seen::Miss(_)));

        // A later failed generation keeps it hidden, as does an abandoned one.
        let Seen::Miss(guard) = look(&store, &key) else {
            panic!()
        };
        guard
            .begin("failed-rerun")
            .unwrap()
            .reject("exit 101", None)
            .unwrap();
        assert!(matches!(look(&store, &key), Seen::Miss(_)));

        // Only a new confirmed success is reused, with its own origin.
        produce(&store, &key, "second-pass", b"second");
        let Seen::Hit(reused) = look(&store, &key) else {
            panic!()
        };
        assert_eq!(reused.stdout(), b"second");
        assert_eq!(reused.provenance().origin_attempt, "second-pass");
        assert_eq!(reused.generation(), 4);
        // The first entry's bytes are retained, unchanged.
        assert_eq!(
            std::fs::read(root.key_dir(&key).join("generations/1/stdout")).unwrap(),
            b"first"
        );
    }

    #[test]
    fn the_lease_spans_a_hit_or_an_attempt_and_excludes_others() {
        let root = Root::new("lease");
        let store = root.store();
        let key = Material::new().key();

        let Seen::Miss(guard) = look(&store, &key) else {
            panic!()
        };
        let pending = guard.begin("owner").unwrap();
        assert!(matches!(store.try_lease(&key).unwrap(), Lease::Busy));
        let captured = pending.capture(original(b"x", b""));
        assert!(matches!(store.try_lease(&key).unwrap(), Lease::Busy));
        captured.confirm(&pass("module::case")).unwrap();

        let Seen::Hit(reused) = look(&store, &key) else {
            panic!()
        };
        // A newer attempt cannot begin while the hit is being consumed.
        assert!(matches!(store.try_lease(&key).unwrap(), Lease::Busy));
        drop(reused);
        assert!(matches!(store.try_lease(&key).unwrap(), Lease::Held(_)));
    }

    #[test]
    fn a_waiting_attempt_starts_only_after_the_holder_settles_and_sees_its_result() {
        let root = Root::new("wait");
        let store = root.store();
        let key = Material::new().key();
        produce(&store, &key, "first", b"pass");

        // A hit is held while a second attempt waits for the lease; the hit is then rejected,
        // as when the current parser does not confirm it.
        let Seen::Hit(reused) = look(&store, &key) else {
            panic!("expected a hit");
        };
        on_one_blocking_thread(async move {
            let waiter = {
                let store = store.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    let guard = store.lease(&key).await.unwrap();
                    match guard.lookup().unwrap() {
                        Lookup::Hit(_) => "hit",
                        Lookup::Miss(guard) => {
                            // The waiter's own attempt fails; nothing older may come back.
                            guard
                                .begin("second")
                                .unwrap()
                                .reject("exit 101", None)
                                .unwrap();
                            "miss"
                        }
                    }
                })
            };
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!waiter.is_finished(), "the waiter overlapped the held hit");
            reject_reused(reused, "not confirmed".to_owned()).await;
            assert_eq!(waiter.await.unwrap(), "miss");
            assert!(matches!(look(&store, &key), Seen::Miss(_)));
        });
    }

    /// Run `scenario` on a runtime whose blocking pool has a single thread, the smallest a
    /// daemon can be configured with. A lease wait that held a blocking thread would starve the
    /// holder's settlement there and never finish; the bound turns that into a failure.
    fn on_one_blocking_thread<T: Send + 'static>(
        scenario: impl std::future::Future<Output = T> + Send + 'static,
    ) -> T {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let finished = runtime
            .block_on(async move { tokio::time::timeout(Duration::from_secs(30), scenario).await });
        runtime.shutdown_timeout(Duration::from_secs(2));
        finished.expect("a same-key scenario did not finish on a one-thread blocking pool")
    }

    fn local_kind() -> CommandExecutionKind {
        CommandExecutionKind::Local {
            digest: ActionDigest::from_content(b"action", CasDigestConfig::testing_default()),
            command: vec!["runner".to_owned()],
            env: Default::default(),
        }
    }

    /// An executor report as the local executor returns it.
    fn report(
        status: CommandExecutionStatus,
        exit_code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> CommandExecutionReport {
        CommandExecutionReport {
            claim: None,
            status,
            timing: buck2_execute::execute::result::CommandExecutionMetadata {
                time_span: buck2_util::time_span::TimeSpan::empty_now(),
                execution_time: Duration::from_millis(7),
                start_time: SystemTime::now(),
                execution_stats: None,
                input_materialization_duration: Duration::ZERO,
                hashing_duration: Duration::ZERO,
                hashed_artifacts_count: 0,
                queue_duration: None,
                suspend_duration: None,
                suspend_count: None,
            },
            std_streams: CommandStdStreams::Local {
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
            },
            exit_code,
            additional_message: None,
            inline_environment_metadata: Default::default(),
        }
    }

    fn succeeded(stdout: &[u8]) -> CommandExecutionReport {
        report(
            CommandExecutionStatus::Success {
                execution_kind: local_kind(),
            },
            Some(0),
            stdout,
            b"",
        )
    }

    fn failed() -> CommandExecutionReport {
        report(
            CommandExecutionStatus::Failure {
                execution_kind: local_kind(),
            },
            Some(101),
            b"failed\0\xff",
            b"panicked",
        )
    }

    /// What a contender saw, once it finished its whole use of the key.
    #[derive(Debug, PartialEq, Eq)]
    enum Contended {
        /// Reused an entry with this stdout, then released it.
        Reused(Vec<u8>),
        /// Ran as a new generation, whose child failed.
        RanAndFailed,
    }

    /// Start `count` same-key contenders through the real admission and settlement path. Each
    /// finishes its use of the key as a case would: a hit is consumed and released, and a miss
    /// runs a child that fails.
    fn contenders(
        path: &str,
        key: &LocalCaseKey,
        reuse: bool,
        count: usize,
    ) -> Vec<tokio::task::JoinHandle<Contended>> {
        (0..count)
            .map(|i| {
                let path = path.to_owned();
                let key = key.clone();
                tokio::spawn(async move {
                    match LocalCaseStart::take(&path, &key, reuse, &format!("contender-{i}"))
                        .await
                        .unwrap()
                    {
                        LocalCaseStart::Hit(reused) => Contended::Reused(reused.stdout().to_vec()),
                        LocalCaseStart::Begun(pending) => {
                            let outcome = settle_execution(pending, &failed()).await;
                            assert!(
                                matches!(outcome.take(), Some(LocalCaseResult::Unsuccessful)),
                                "a failure was captured"
                            );
                            Contended::RanAndFailed
                        }
                    }
                })
            })
            .collect()
    }

    async fn begun(path: &str, key: &LocalCaseKey, reuse: bool, attempt: &str) -> PendingCase {
        match LocalCaseStart::take(path, key, reuse, attempt)
            .await
            .unwrap()
        {
            LocalCaseStart::Begun(pending) => pending,
            LocalCaseStart::Hit(_) => panic!("{attempt}: unexpected hit"),
        }
    }

    /// Let contenders reach their lease wait, and check none overlapped the holder.
    async fn all_waiting(waiters: &[tokio::task::JoinHandle<Contended>]) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            waiters.iter().all(|w| !w.is_finished()),
            "a contender overlapped the lease holder"
        );
    }

    async fn finished(waiters: Vec<tokio::task::JoinHandle<Contended>>) -> Vec<Contended> {
        let mut seen = Vec::new();
        for waiter in waiters {
            seen.push(waiter.await.unwrap());
        }
        seen
    }

    #[test]
    fn same_key_contenders_all_finish_on_a_one_thread_blocking_pool() {
        let root = Root::new("one-thread");
        let path = root.0.to_str().unwrap().to_owned();
        let key = Material::new().key();
        let state_path = root.key_dir(&key).join("state.json");
        on_one_blocking_thread(async move {
            // Confirmed success: the holder publishes while two others wait; both then reuse it.
            let pending = begun(&path, &key, true, "success-holder").await;
            let waiters = contenders(&path, &key, true, 2);
            all_waiting(&waiters).await;
            let outcome = settle_execution(pending, &succeeded(b"ok\0\xff tail")).await;
            settle_fresh(outcome.take(), Ok(pass("module::case"))).await;
            let reused = Contended::Reused(b"ok\0\xff tail".to_vec());
            assert_eq!(
                finished(waiters).await,
                [reused, Contended::Reused(b"ok\0\xff tail".to_vec())]
            );

            // Rejected hit: the holder's hit is not confirmed while two others wait; neither may
            // reuse it, and each runs and fails as a new generation.
            let LocalCaseStart::Hit(reused) = LocalCaseStart::take(&path, &key, true, "hit-holder")
                .await
                .unwrap()
            else {
                panic!("expected the published hit");
            };
            let waiters = contenders(&path, &key, true, 2);
            all_waiting(&waiters).await;
            reject_reused(reused, "not confirmed".to_owned()).await;
            assert_eq!(
                finished(waiters).await,
                [Contended::RanAndFailed, Contended::RanAndFailed]
            );

            // Failed execution: after a new success, a tracked rerun that does not read the
            // store fails while two others wait. Its newer unsuccessful generation hides the
            // older success from both.
            let pending = begun(&path, &key, true, "second-success").await;
            let outcome = settle_execution(pending, &succeeded(b"second")).await;
            settle_fresh(outcome.take(), Ok(pass("module::case"))).await;
            let pending = begun(&path, &key, false, "failing-rerun").await;
            let waiters = contenders(&path, &key, true, 2);
            all_waiting(&waiters).await;
            assert!(
                settle_execution(pending, &failed())
                    .await
                    .take()
                    .is_some_and(|r| matches!(r, LocalCaseResult::Unsuccessful))
            );
            assert_eq!(
                finished(waiters).await,
                [Contended::RanAndFailed, Contended::RanAndFailed]
            );

            // Cancelled waiter: abandoning a wait leaves nothing holding or waiting on the key.
            let pending = begun(&path, &key, true, "cancel-holder").await;
            let waiter = contenders(&path, &key, true, 1).pop().unwrap();
            all_waiting(std::slice::from_ref(&waiter)).await;
            waiter.abort();
            match waiter.await {
                Err(e) => assert!(e.is_cancelled()),
                Ok(_) => panic!("the cancelled waiter took the lease"),
            }
            assert!(
                settle_execution(pending, &failed())
                    .await
                    .take()
                    .is_some_and(|r| matches!(r, LocalCaseResult::Unsuccessful))
            );
            let pending = begun(&path, &key, true, "after-cancel").await;
            assert!(
                settle_execution(pending, &failed())
                    .await
                    .take()
                    .is_some_and(|r| matches!(r, LocalCaseResult::Unsuccessful))
            );
        });
        // Generations: 1 succeeded; the rejected hit hid it as 2; 3 and 4 failed; 5 succeeded;
        // 6, 7 and 8 failed; 9 and 10 failed around the cancelled waiter, which began none.
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
        assert_eq!(state["latest_generation"], 10, "{state}");
        assert_eq!(state["latest_state"], "unsuccessful", "{state}");
        assert_eq!(state["success_generation"], 5, "{state}");
    }

    /// A tracked case keeps its fresh-directory scratch path from before its child runs until its
    /// generation is published or rejected, through capture and confirmation. Another key of the
    /// same case (the same scratch path), tracked or not, cannot take the path, and so cannot
    /// remove it, in between; the path's lease is free once the generation is settled.
    #[test]
    fn a_tracked_case_holds_its_scratch_path_until_its_generation_is_settled() {
        use buck2_core::cells::CellResolver;
        use buck2_core::cells::cell_root_path::CellRootPathBuf;
        use buck2_core::cells::name::CellName;
        use buck2_core::configuration::data::ConfigurationData;
        use buck2_core::fs::artifact_path_resolver::ArtifactFs;
        use buck2_core::fs::buck_out_path::BuckOutPathResolver;
        use buck2_core::fs::project::ProjectRootTemp;
        use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
        use buck2_core::provider::label::ConfiguredProvidersLabel;
        use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
        use dupe::Dupe;

        let root = Root::new("scratch-lease");
        let path = root.0.to_str().unwrap().to_owned();
        let temp = ProjectRootTemp::new().unwrap();
        let fs = ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".into())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            temp.path().dupe(),
        );
        let target = ConfiguredProvidersLabel::new(
            ConfiguredTargetLabel::testing_parse("cell//pkg:t", ConfigurationData::testing_new()),
            Default::default(),
        );
        let stage = TestStage::Testing {
            suite: "suite".to_owned(),
            testcases: vec!["module::case".to_owned()],
            variant: None,
            repeat_count: None,
        };
        let fresh =
            FreshDirs::for_stage(&target, &stage, ["FRESH_A"], &fs).unwrap().unwrap();
        // Two keys of one case: the second differs only in an unrelated variable.
        let key = Material::new().key();
        let mut other = Material::new();
        other.env.push((OsString::from("OTHER"), OsString::from("1")));
        let other = other.key();
        assert_ne!(key.digest(), other.digest());

        on_one_blocking_thread(async move {
            let short = Duration::from_millis(200);
            for confirmed in [true, false] {
                let pending = begun(&path, &key, false, "holder")
                    .await
                    .holding(fresh.lease().await);
                // The other key's own lease is free, but the scratch path is not.
                let other_pending = begun(&path, &other, false, "other-key").await;
                assert!(
                    tokio::time::timeout(short, fresh.lease()).await.is_err(),
                    "the scratch path was free while its holder's child could run"
                );
                // A local exit-0 success is captured; the path stays held until it is settled.
                let outcome = settle_execution(pending, &succeeded(b"ok")).await;
                assert!(
                    tokio::time::timeout(short, fresh.lease()).await.is_err(),
                    "the scratch path was free before the captured generation was settled"
                );
                let confirmation = if confirmed {
                    Ok(pass("module::case"))
                } else {
                    Err(NotConfirmed::NotPass)
                };
                settle_fresh(outcome.take(), confirmation).await;
                let lease = tokio::time::timeout(short, fresh.lease())
                    .await
                    .expect("the settled generation kept its scratch path");
                drop(lease);
                drop(other_pending);
            }
            // A failed child records its generation unsuccessful, which also frees the path.
            let pending = begun(&path, &key, false, "failing")
                .await
                .holding(fresh.lease().await);
            assert!(tokio::time::timeout(short, fresh.lease()).await.is_err());
            assert!(
                settle_execution(pending, &failed())
                    .await
                    .take()
                    .is_some_and(|r| matches!(r, LocalCaseResult::Unsuccessful))
            );
            tokio::time::timeout(short, fresh.lease())
                .await
                .expect("the unsuccessful generation kept its scratch path");
        });
    }

    #[test]
    fn an_unconfirmed_fresh_capture_publishes_nothing_and_keeps_its_raw() {
        let root = Root::new("unconfirmed");
        let path = root.0.to_str().unwrap().to_owned();
        let key = Material::new().key();
        let key_dir = root.key_dir(&key);
        on_one_blocking_thread(async move {
            let pending = begun(&path, &key, true, "first").await;
            let outcome = settle_execution(pending, &succeeded(b"pass")).await;
            settle_fresh(outcome.take(), Ok(pass("module::case"))).await;
            // Each parse the runner reports as an explicit FAIL is settled as unconfirmed.
            for (i, not_confirmed) in [
                NotConfirmed::NotExactlyOneResult(0),
                NotConfirmed::DifferentCase,
                NotConfirmed::NotExactlyOneResult(2),
            ]
            .into_iter()
            .enumerate()
            {
                let pending = begun(&path, &key, i > 0, &format!("unconfirmed-{i}")).await;
                let raw = format!("exit 0, unconfirmed {i}\0\u{fffd}");
                let outcome = settle_execution(pending, &succeeded(raw.as_bytes())).await;
                settle_fresh(outcome.take(), Err(not_confirmed)).await;
            }
            let _hidden = begun(&path, &key, true, "after").await;
        });
        for (generation, i) in [(2, 0), (3, 1), (4, 2)] {
            let entry = key_dir.join(format!("generations/{generation}"));
            assert_eq!(
                std::fs::read(entry.join("stdout")).unwrap(),
                format!("exit 0, unconfirmed {i}\0\u{fffd}").as_bytes()
            );
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(entry.join("manifest.json")).unwrap())
                    .unwrap();
            assert_eq!(manifest["outcome"], "unsuccessful");
            assert_eq!(manifest["confirmed_status"], serde_json::Value::Null);
        }
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(key_dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(state["success_generation"], 1);
        assert_eq!(state["latest_generation"], 5);
    }

    #[test]
    fn a_cancelled_execution_keeps_its_exact_raw_and_publishes_nothing() {
        let root = Root::new("cancelled");
        let path = root.0.to_str().unwrap().to_owned();
        let key = Material::new().key();
        let key_dir = root.key_dir(&key);
        let stdout = b"partial\0out \xfe\xff unterminated".to_vec();
        let stderr = b"err\0\x80 no newline".to_vec();
        let (out, err) = (stdout.clone(), stderr.clone());
        on_one_blocking_thread(async move {
            let pending = begun(&path, &key, true, "first").await;
            let outcome = settle_execution(pending, &succeeded(b"pass")).await;
            settle_fresh(outcome.take(), Ok(pass("module::case"))).await;

            // The local executor's report for a child cancelled after writing these bytes.
            let pending = begun(&path, &key, false, "cancelled").await;
            let cancelled = report(
                CommandExecutionStatus::Cancelled {
                    execution_kind: local_kind(),
                    reason: None,
                },
                None,
                &out,
                &err,
            );
            assert!(
                settle_execution(pending, &cancelled)
                    .await
                    .take()
                    .is_some_and(|r| matches!(r, LocalCaseResult::Unsuccessful))
            );
            // The older success stays hidden.
            let pending = begun(&path, &key, true, "after").await;
            drop(pending);
        });
        let generation = key_dir.join("generations/2");
        assert_eq!(std::fs::read(generation.join("stdout")).unwrap(), stdout);
        assert_eq!(std::fs::read(generation.join("stderr")).unwrap(), stderr);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(generation.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["outcome"], "unsuccessful");
        assert_eq!(manifest["exit_code"], serde_json::Value::Null);
        assert_eq!(manifest["confirmed_status"], serde_json::Value::Null);
        assert!(
            manifest["reason"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("cancel"),
            "{manifest}"
        );
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(key_dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(state["latest_generation"], 3);
        assert_eq!(state["latest_state"], "unsuccessful");
        assert_eq!(state["success_generation"], 1);
    }

    #[test]
    fn the_root_contract_refuses_symlinks_shared_ancestors_and_redirection() {
        use std::os::unix::fs::symlink;
        let base = Root::new("contract");
        let path = |p: &std::path::Path| p.to_str().unwrap().to_owned();
        let real = base.0.join("real");
        let key = Material::new().key();
        let refused = |spelling: String, why: &str| {
            assert!(
                LocalCaseStore::open(&spelling).is_err(),
                "{why}: `{spelling}` was accepted"
            );
        };

        // A legitimate root below private and root-owned ancestors.
        let store = LocalCaseStore::open(&path(&real)).unwrap();
        produce(&store, &key, "old-pass", b"old pass");

        // Symlinks: the final component with and without a trailing slash, and an ancestor.
        let link = base.0.join("link");
        symlink(&real, &link).unwrap();
        refused(path(&link), "final symlink");
        refused(
            format!("{}/", path(&link)),
            "final symlink with trailing slash",
        );
        refused(format!("{}/", path(&real)), "trailing slash");
        let alias = base.0.join("alias");
        symlink(&base.0, &alias).unwrap();
        refused(path(&alias.join("real")), "ancestor symlink");
        // Spellings that are not lexically canonical.
        refused(format!("{}/./real", path(&base.0)), "`.` component");
        refused(format!("{}/x/../real", path(&base.0)), "`..` component");
        refused(format!("{}//real", path(&base.0)), "empty component");
        refused("/".to_owned(), "filesystem root");
        refused("relative/root".to_owned(), "relative");

        // An ancestor another principal may write, even if sticky, unless owned by root.
        let shared = base.0.join("shared");
        std::fs::create_dir(&shared).unwrap();
        for mode in [0o777, 0o1777, 0o770, 0o720] {
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(mode)).unwrap();
            refused(
                path(&shared.join("root")),
                &format!("ancestor mode {mode:o}"),
            );
        }
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        LocalCaseStore::open(&path(&shared.join("root"))).unwrap();

        // Redirection while a lease is held. Another retained namespace holds an older success
        // for the same key; the pinned namespace has a pending generation under a held lease.
        let other = base.0.join("other");
        produce(
            &LocalCaseStore::open(&path(&other)).unwrap(),
            &key,
            "older",
            b"older",
        );
        let Seen::Hit(reused) = look(&store, &key) else {
            panic!("expected the pinned success");
        };
        reused.reject("not confirmed").unwrap();
        let Seen::Miss(guard) = look(&store, &key) else {
            panic!("a rejected success was reused");
        };
        let pending = guard.begin("held").unwrap();
        // Reopening the same spelling yields the same namespace, whose lease is held.
        let reopened = LocalCaseStore::open(&path(&real)).unwrap();
        assert!(matches!(reopened.try_lease(&key).unwrap(), Lease::Busy));
        // The spelling is redirected to the other namespace: it is refused, not switched.
        std::fs::rename(&real, base.0.join("real-before")).unwrap();
        std::fs::rename(&other, &real).unwrap();
        refused(path(&real), "redirected while a lease is held");
        pending.reject("exit 101", None).unwrap();
        refused(path(&real), "redirected after the lease was released");
        // The pinned namespace is still the one used, and its older success stays hidden.
        assert!(matches!(look(&reopened, &key), Seen::Miss(_)));
    }

    #[test]
    fn a_case_ineligible_at_its_launch_check_runs_its_legacy_request() {
        use buck2_common::file_ops::metadata::FileMetadata;
        use buck2_common::file_ops::metadata::TrackedFileDigest;
        use buck2_core::cells::CellResolver;
        use buck2_core::cells::cell_root_path::CellRootPathBuf;
        use buck2_core::cells::name::CellName;
        use buck2_core::fs::artifact_path_resolver::ArtifactFs;
        use buck2_core::fs::buck_out_path::BuckOutPathResolver;
        use buck2_core::fs::project::ProjectRootTemp;
        use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
        use buck2_directory::directory::entry::DirectoryEntry;
        use buck2_execute::digest_config::DigestConfig;
        use buck2_execute::directory::ActionDirectoryMember;
        use buck2_execute::execute::environment_inheritance::EnvironmentInheritance;
        use buck2_execute::execute::request::CommandExecutionInput;
        use buck2_execute::execute::request::CommandExecutionPaths;
        use dupe::Dupe;

        let temp = ProjectRootTemp::new().unwrap();
        let project = temp.path();
        let digest_config = DigestConfig::testing_default();
        let fs = ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".into())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            project.dupe(),
        );
        // A launched build-artifact script whose interpreter is a host tool, which only the
        // launch check after materialization can see.
        let script = "buck-out/v2/gen/t/script";
        let body = b"#!/bin/sh\necho\n";
        let disk = project.root().as_path().join(script);
        std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
        std::fs::write(&disk, body).unwrap();
        let store = std::fs::canonicalize("/bin/sh").unwrap();
        assert!(
            store.starts_with("/nix/store"),
            "the test needs a Nix store host"
        );
        let store = store.parent().unwrap().to_str().unwrap().to_owned();
        let make = |args: Vec<&str>| {
            let input = CommandExecutionInput::IncrementalRemoteOutput(
                ProjectRelativePathBuf::unchecked_new(script.to_owned()),
                DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                    digest: TrackedFileDigest::from_content(
                        body,
                        digest_config.cas_digest_config(),
                    ),
                    is_executable: true,
                })),
            );
            let paths = CommandExecutionPaths::new(
                vec![input],
                Default::default(),
                &fs,
                digest_config,
                None,
            )
            .unwrap();
            let mut env = sorted_vector_map::SortedVectorMap::new();
            env.insert("PATH".to_owned(), store.clone());
            CommandExecutionRequest::new(
                vec![],
                args.into_iter().map(str::to_owned).collect(),
                paths,
                env,
            )
            .with_local_environment_inheritance(EnvironmentInheritance::empty())
            .with_executor_preference(ExecutorPreference::LocalPreferred)
        };
        let facts = || RequestFacts {
            project_fs: project,
            buck_out: vec!["buck-out".to_owned(), "v2".to_owned()],
        };
        let cache_root = project.root().as_path().join("cache-root");
        let candidate = || LocalCaseCandidate {
            root: cache_root.to_str().unwrap().to_owned(),
            reuse: true,
            case: Material::new().case,
        };
        let legacy_policy = |request: &CommandExecutionRequest| {
            (
                request.executor_preference(),
                request.omit_local_invocation_identity_env(),
                request.all_args_vec(),
                request.env().clone(),
                determined_local_environment(request, project.root(), None).is_some(),
            )
        };

        let args = vec![script, "module::case"];
        let (constrained, case) = candidate().constrain(make(args.clone()), facts(), None, |_| true);
        let case = case.expect("the case is eligible before its launch check");
        assert_eq!(
            constrained.executor_preference(),
            ExecutorPreference::LocalRequired
        );
        assert!(constrained.omit_local_invocation_identity_env());
        let why = case.launch().verify().unwrap_err();
        assert!(why.contains("/bin/sh"), "{why}");
        let fallback = case.into_legacy_request(constrained);
        // The same request, environment and executor policy as a case that never opted in.
        assert_eq!(legacy_policy(&fallback), legacy_policy(&make(args)));
        assert_eq!(
            fallback.executor_preference(),
            ExecutorPreference::LocalPreferred
        );
        assert!(!fallback.omit_local_invocation_identity_env());

        // An earlier classification failure gives back the same legacy request.
        let args = vec![script, "module::case", "/etc/hostname"];
        let (earlier, case) = candidate().constrain(make(args.clone()), facts(), None, |_| true);
        assert!(case.is_err());
        assert_eq!(legacy_policy(&earlier), legacy_policy(&make(args)));
        // Neither touched the store.
        assert!(!cache_root.exists());
    }

    /// The only scratch path a case may carry is the engine's own fresh-directory scratch path,
    /// set up exactly as the engine sets it up for that case. The key then records the
    /// environment the executor installs: `TMPDIR` set to that path, then each declared name set
    /// to its directory. Any other scratch path keeps the case out of the store.
    #[test]
    fn only_the_engines_fresh_scratch_path_is_admitted_and_keyed() {
        use buck2_common::file_ops::metadata::FileMetadata;
        use buck2_common::file_ops::metadata::TrackedFileDigest;
        use buck2_core::cells::CellResolver;
        use buck2_core::cells::cell_root_path::CellRootPathBuf;
        use buck2_core::cells::name::CellName;
        use buck2_core::configuration::data::ConfigurationData;
        use buck2_core::fs::artifact_path_resolver::ArtifactFs;
        use buck2_core::fs::buck_out_path::BuckOutPathResolver;
        use buck2_core::fs::project::ProjectRootTemp;
        use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
        use buck2_core::provider::label::ConfiguredProvidersLabel;
        use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
        use buck2_directory::directory::entry::DirectoryEntry;
        use buck2_execute::digest_config::DigestConfig;
        use buck2_execute::directory::ActionDirectoryMember;
        use buck2_execute::execute::environment_inheritance::EnvironmentInheritance;
        use buck2_execute::execute::request::CommandExecutionInput;
        use buck2_execute::execute::request::CommandExecutionPaths;
        use dupe::Dupe;

        let temp = ProjectRootTemp::new().unwrap();
        let project = temp.path();
        let digest_config = DigestConfig::testing_default();
        let fs = ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".into())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            project.dupe(),
        );
        let target = ConfiguredProvidersLabel::new(
            ConfiguredTargetLabel::testing_parse("cell//pkg:t", ConfigurationData::testing_new()),
            Default::default(),
        );
        let stage = |case: &str| TestStage::Testing {
            suite: "suite".to_owned(),
            testcases: vec![case.to_owned()],
            variant: None,
            repeat_count: None,
        };
        let names = ["FRESH_A", "FRESH_B"];
        let fresh = FreshDirs::for_stage(&target, &stage("module::case"), names, &fs)
            .unwrap()
            .unwrap();
        let foreign = FreshDirs::for_stage(&target, &stage("module::other"), names, &fs)
            .unwrap()
            .unwrap();
        let store = std::fs::canonicalize("/bin/sh").unwrap();
        assert!(
            store.starts_with("/nix/store"),
            "the test needs a Nix store host"
        );
        let store = store.parent().unwrap().to_str().unwrap().to_owned();
        let script = "buck-out/v2/gen/t/runner";
        let body = b"#!/bin/sh\necho\n";
        // The request the orchestrator builds for a case whose scratch path is `scratch`.
        let make = |scratch: &FreshDirs| {
            let runner = CommandExecutionInput::IncrementalRemoteOutput(
                ProjectRelativePathBuf::unchecked_new(script.to_owned()),
                DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                    digest: TrackedFileDigest::from_content(
                        body,
                        digest_config.cas_digest_config(),
                    ),
                    is_executable: true,
                })),
            );
            let paths = CommandExecutionPaths::new(
                vec![runner, scratch.input()],
                Default::default(),
                &fs,
                digest_config,
                None,
            )
            .unwrap();
            let mut env: sorted_vector_map::SortedVectorMap<String, String> =
                sorted_vector_map::SortedVectorMap::new();
            env.insert("PATH".to_owned(), store.clone());
            CommandExecutionRequest::new(
                vec![],
                vec![script.to_owned(), "module::case".to_owned()],
                paths,
                env,
            )
            .with_local_environment_inheritance(EnvironmentInheritance::empty())
            .with_executor_preference(ExecutorPreference::LocalPreferred)
            .with_scratch_dir_variables(scratch.variables())
        };
        let facts = || RequestFacts {
            project_fs: project,
            buck_out: vec!["buck-out".to_owned(), "v2".to_owned()],
        };
        let cache_root = project.root().as_path().join("cache-root");
        let candidate = || LocalCaseCandidate {
            root: cache_root.to_str().unwrap().to_owned(),
            reuse: true,
            case: Material::new().case,
        };

        // Another case's scratch path, or a scratch path without the engine's setup, is refused
        // and the request given back unchanged.
        let (refused, case) =
            candidate().constrain(make(&foreign), facts(), Some(&fresh), |_| true);
        assert_eq!(case.err(), Some(LocalCaseGap::ScratchPath));
        assert_eq!(refused.executor_preference(), ExecutorPreference::LocalPreferred);
        let (_, case) = candidate().constrain(make(&fresh), facts(), None, |_| true);
        assert_eq!(case.err(), Some(LocalCaseGap::ScratchPath));

        let (constrained, case) =
            candidate().constrain(make(&fresh), facts(), Some(&fresh), |_| true);
        let case = case.expect("the engine's own fresh scratch path is admitted");
        assert_eq!(
            constrained.executor_preference(),
            ExecutorPreference::LocalRequired
        );
        let action_digest =
            ActionDigest::from_content(b"action", digest_config.cas_digest_config());
        let working_directory = project.resolve(constrained.working_directory());
        let key = case
            .key(
                &constrained,
                PreparedFacts {
                    action_digest: &action_digest,
                    working_directory: &working_directory,
                    network_access: "None".to_owned(),
                    digest_config: digest_config.cas_digest_config(),
                },
            )
            .unwrap();
        // The recorded environment, in installation order: the scratch directory variables last.
        let mut expected = KeyRecord::default();
        let scratch = fresh.absolute().to_string();
        let env = fresh.values().unwrap();
        for (name, value) in [
            ("TMPDIR", scratch.as_str()),
            ("PATH", store.as_str()),
            ("FRESH_A", env[0].1.as_str()),
            ("FRESH_B", env[1].1.as_str()),
        ] {
            expected.field("env_name", name.as_bytes());
            expected.field("env_value", value.as_bytes());
        }
        assert!(
            key.record
                .windows(expected.0.len())
                .any(|window| window == expected.0.as_slice()),
            "the key does not record TMPDIR and the fresh directories in installation order"
        );
        assert_eq!(env[0].1, format!("{scratch}/0"));
        // Neither touched the store.
        assert!(!cache_root.exists());
    }

    #[test]
    fn every_key_field_distinguishes_entries() {
        let base = Material::new().key();
        let mut changed: Vec<(&str, LocalCaseKey)> = Vec::new();
        let mut push = |name, f: &dyn Fn(&mut Material)| {
            let mut m = Material::new();
            f(&mut m);
            changed.push((name, m.key()));
        };
        push("case", &|m| m.case.case = "module::other".to_owned());
        push("target", &|m| m.case.target = "root//pkg:other (cfg)".to_owned());
        push("suite", &|m| m.case.suite = "root//pkg:other".to_owned());
        push("discovery", &|m| m.case.discovery = "changed".to_owned());
        push("action", &|m| m.action = "changed".to_owned());
        push("argv", &|m| m.argv.push("--nocapture".to_owned()));
        push("cwd", &|m| m.cwd = "/project/sub".to_owned());
        push("tmpdir value", &|m| m.env[1].1 = OsString::from("/tmp/b"));
        push("tmpdir absent", &|m| {
            m.env.remove(1);
        });
        push("tmpdir empty", &|m| m.env[1].1 = OsString::new());
        push("pwd", &|m| m.env[2].1 = OsString::from("/elsewhere"));
        push("extra variable", &|m| {
            m.env
                .push((OsString::from("UNDECLARED"), OsString::from("1")))
        });
        push("non-utf8 value", &|m| {
            use std::os::unix::ffi::OsStringExt;
            m.env[0].1 = OsString::from_vec(b"/bin\xff".to_vec())
        });
        push("env order", &|m| m.env.swap(0, 1));
        push("timeout", &|m| m.timeout = Some(Duration::from_secs(61)));
        push("no timeout", &|m| m.timeout = None);
        push("network", &|m| m.network = "Some(All)".to_owned());
        push("engine", &|m| m.engine = "other-engine".to_owned());
        // Field boundaries cannot be forged by moving bytes between adjacent fields.
        push("boundary", &|m| {
            m.argv = vec!["runnermodule::case".to_owned(), String::new()]
        });

        assert_eq!(base.digest(), Material::new().key().digest());
        let mut digests = std::collections::HashSet::new();
        digests.insert(base.digest().to_owned());
        for (name, key) in &changed {
            assert!(
                digests.insert(key.digest().to_owned()),
                "{name} did not change the key"
            );
        }
    }

    #[test]
    fn a_changed_key_is_a_miss_and_does_not_disturb_the_original_entry() {
        let root = Root::new("invalidate");
        let store = root.store();
        let key = Material::new().key();
        produce(&store, &key, "original", b"pass");
        let mut changed = Material::new();
        changed.action = "rebuilt test binary".to_owned();
        assert!(matches!(look(&store, &changed.key()), Seen::Miss(_)));
        assert!(matches!(look(&store, &key), Seen::Hit(_)));
    }

    #[test]
    fn corrupt_substituted_or_shared_entries_are_errors_not_hits() {
        let key = Material::new().key();

        let cases: Vec<(&str, &dyn Fn(&std::path::Path))> = vec![
            ("changed stdout", &|entry| {
                std::fs::write(entry.join("stdout"), b"tampered").unwrap()
            }),
            ("symlinked stdout", &|entry| {
                std::fs::rename(entry.join("stdout"), entry.join("real")).unwrap();
                std::os::unix::fs::symlink(entry.join("real"), entry.join("stdout")).unwrap();
            }),
            ("fifo stdout", &|entry| {
                std::fs::remove_file(entry.join("stdout")).unwrap();
                let path = std::ffi::CString::new(
                    entry.join("stdout").as_os_str().as_encoded_bytes(),
                )
                .unwrap();
                // SAFETY: valid path.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }),
            ("hard-linked stdout", &|entry| {
                std::fs::hard_link(entry.join("stdout"), entry.join("alias")).unwrap()
            }),
            ("readable by others", &|entry| {
                std::fs::set_permissions(
                    entry.join("stdout"),
                    std::fs::Permissions::from_mode(0o644),
                )
                .unwrap()
            }),
            ("manifest for another key", &|entry| {
                let manifest = std::fs::read_to_string(entry.join("manifest.json")).unwrap();
                let manifest = manifest.replace("\"generation\": 1", "\"generation\": 7");
                std::fs::remove_file(entry.join("manifest.json")).unwrap();
                std::fs::write(entry.join("manifest.json"), manifest).unwrap();
                std::fs::set_permissions(
                    entry.join("manifest.json"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }),
        ];
        for (name, damage) in cases {
            let root = Root::new("corrupt");
            let store = root.store();
            produce(&store, &key, "original", b"pass");
            let entry = root.key_dir(&key).join("generations/1");
            damage(&entry);
            let lookup = held(&store, &key).0.lookup();
            assert!(lookup.is_err(), "{name} was not refused");
            // The evidence is left in place.
            assert!(entry.join("manifest.json").exists(), "{name}");
        }
    }

    #[test]
    fn the_root_must_be_private_and_absolute() {
        let root = Root::new("public");
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(LocalCaseStore::open(root.0.to_str().unwrap()).is_err());
        assert!(LocalCaseStore::open("relative/root").is_err());
        let link = root.0.with_extension("link");
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&root.0, &link).unwrap();
        assert!(LocalCaseStore::open(link.to_str().unwrap()).is_err());
        std::fs::remove_file(&link).unwrap();
    }

    #[test]
    fn only_exactly_one_pass_for_the_expected_case_is_confirmed() {
        let pass = TestStatus::PASS;
        let fail = TestStatus::FAIL;
        let skip = TestStatus::SKIP;
        assert!(ConfirmedPass::exact("a", 0, &[("a", &pass)]).is_ok());
        assert_eq!(
            ConfirmedPass::exact("a", 0, &[]),
            Err(NotConfirmed::NotExactlyOneResult(0))
        );
        assert_eq!(
            ConfirmedPass::exact("a", 0, &[("a", &pass), ("a", &pass)]),
            Err(NotConfirmed::NotExactlyOneResult(2))
        );
        assert_eq!(
            ConfirmedPass::exact("a", 0, &[("b", &pass)]),
            Err(NotConfirmed::DifferentCase)
        );
        assert_eq!(
            ConfirmedPass::exact("a", 0, &[("a", &fail)]),
            Err(NotConfirmed::NotPass)
        );
        assert_eq!(
            ConfirmedPass::exact("a", 0, &[("a", &skip)]),
            Err(NotConfirmed::NotPass)
        );
        assert_eq!(
            ConfirmedPass::exact("a", 1, &[("a", &pass)]),
            Err(NotConfirmed::NonZeroExit)
        );
    }

    #[test]
    fn admission_requires_request_store_one_case_and_discovery() {
        let testing = |cases: Vec<&str>, variant: Option<&str>| TestStage::Testing {
            suite: "suite".to_owned(),
            testcases: cases.into_iter().map(str::to_owned).collect(),
            variant: variant.map(str::to_owned),
            repeat_count: None,
        };
        let discovery = LocalCaseRequest::Track {
            discovery: DiscoveryIdentity::new("rust", "run", "list", "parsers", b"a\nb\n"),
            reuse: true,
        };
        let unsupported = LocalCaseRequest::Unsupported(LocalCaseGap::ParserNotSelfContained);
        let admit = |stage: &TestStage, requested, disabled, root: Option<&str>, discovery| {
            LocalCaseAdmission::for_stage(stage, "target", requested, disabled, root, discovery)
        };
        let one = testing(vec!["a"], None);
        assert!(matches!(
            admit(&one, true, false, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::Candidate(_)
        ));
        assert!(matches!(
            admit(&one, false, false, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::NotRequested
        ));
        // Disabling caching still records a generation, without reading the store.
        assert!(matches!(
            admit(&one, true, true, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::Candidate(LocalCaseCandidate { reuse: false, .. })
        ));
        assert!(matches!(
            admit(&one, true, false, Some("/r"), Some(&unsupported)),
            LocalCaseAdmission::Ineligible(LocalCaseGap::ParserNotSelfContained)
        ));
        assert!(matches!(
            admit(&one, true, false, None, Some(&discovery)),
            LocalCaseAdmission::Ineligible(LocalCaseGap::NoStoreConfigured)
        ));
        assert!(matches!(
            admit(&one, true, false, Some("/r"), None),
            LocalCaseAdmission::Refused(LocalCaseGap::NoDiscoveryIdentity)
        ));
        assert!(matches!(
            admit(&testing(vec!["a", "b"], None), true, false, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::Refused(LocalCaseGap::NotOneExactCase)
        ));
        assert!(matches!(
            admit(&testing(vec![], None), true, false, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::Refused(LocalCaseGap::NotOneExactCase)
        ));
        assert!(matches!(
            admit(&testing(vec!["a"], Some("v")), true, false, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::Refused(LocalCaseGap::NotOneExactCase)
        ));
        let listing = TestStage::Listing {
            suite: "suite".to_owned(),
            cacheable: true,
        };
        assert!(matches!(
            admit(&listing, true, false, Some("/r"), Some(&discovery)),
            LocalCaseAdmission::NotRequested
        ));
    }

    #[test]
    fn discovery_identity_covers_provider_parsers_and_listing() {
        let base = DiscoveryIdentity::new("rust", "run", "list", "parsers", b"a\nb\n");
        for changed in [
            DiscoveryIdentity::new("custom", "run", "list", "parsers", b"a\nb\n"),
            DiscoveryIdentity::new("rust", "run2", "list", "parsers", b"a\nb\n"),
            DiscoveryIdentity::new("rust", "run", "list2", "parsers", b"a\nb\n"),
            DiscoveryIdentity::new("rust", "run", "list", "parsers2", b"a\nb\n"),
            DiscoveryIdentity::new("rust", "run", "list", "parsers", b"a\nb\nc\n"),
        ] {
            assert_ne!(base, changed);
        }
        assert_eq!(
            base,
            DiscoveryIdentity::new("rust", "run", "list", "parsers", b"a\nb\n")
        );
    }
}
