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
//!   inaccessible to others; no default location exists;
//! - the request runs exactly one listed case, with no variant or repeat;
//! - the child's whole environment is decided by the request: the inheritance clears the daemon
//!   environment, there is no scratch path, worker or local resource, and the per-invocation
//!   identity variables are not injected (`determined_local_environment`);
//! - the request declares no outputs, every file its command names (`argv[0]`, a launched
//!   script's `#!` interpreter, every `PATH` entry, and any argument or environment value naming
//!   an existing file) is in the input tree or the Nix store, every input-tree file is a build
//!   artifact under `buck-out` rather than a checkout file, and no loader override is set
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
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_core::fs::project::ProjectRoot;
use buck2_execute_impl::executors::local::determined_local_environment;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_test_api::data::TestStage;
use buck2_test_api::data::TestStatus;
use serde::Deserialize;
use serde::Serialize;

use crate::local_case_closure::InputTree;
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

fn sha256(bytes: &[u8]) -> String {
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
    pub(crate) fn constrain(
        self,
        request: CommandExecutionRequest,
        facts: RequestFacts<'_>,
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
        }) {
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
        let request = request.with_omit_local_invocation_identity_env(true);
        let launch = match Self::launch_plan(&request, &facts) {
            Ok(launch) => launch,
            Err(gap) => return (request.with_omit_local_invocation_identity_env(false), Err(gap)),
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
            }),
        )
    }

    /// Classify every file the constrained request's command names.
    fn launch_plan(
        request: &CommandExecutionRequest,
        facts: &RequestFacts<'_>,
    ) -> Result<LaunchPlan, LocalCaseGap> {
        let working_directory = facts.project_fs.resolve(request.working_directory());
        let env = determined_local_environment(request, &working_directory)
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

/// An eligible case whose request has been constrained to the reuse contract.
pub(crate) struct ConstrainedCase {
    root: String,
    reuse: bool,
    case: CaseIdentity,
    launch: LaunchPlan,
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

    /// Build the full key for the constrained request.
    pub(crate) fn key(
        &self,
        request: &CommandExecutionRequest,
        prepared: PreparedFacts<'_>,
    ) -> Result<LocalCaseKey, LocalCaseGap> {
        let environment = determined_local_environment(request, prepared.working_directory)
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
struct KeyRecord(Vec<u8>);

impl KeyRecord {
    fn field(&mut self, name: &str, value: &[u8]) {
        self.0.extend((name.len() as u64).to_be_bytes());
        self.0.extend(name.as_bytes());
        self.0.extend((value.len() as u64).to_be_bytes());
        self.0.extend(value);
    }

    fn count(&mut self, name: &str, count: usize) {
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

    impl Dir {
        fn open_root(path: &str) -> buck2_error::Result<Self> {
            if !path.starts_with('/') {
                return Err(store_error(format!(
                    "[test] local_case_result_cache_root must be absolute, got `{path}`"
                )));
            }
            let c_path = CString::new(path)
                .map_err(|_| store_error(format!("`{path}` contains NUL")))?;
            // SAFETY: valid C string; mkdir reports failure through errno.
            if unsafe { libc::mkdir(c_path.as_ptr(), 0o700) } != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(io_error("Creating", path));
            }
            // SAFETY: valid C string; the returned descriptor is owned below.
            let fd = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io_error("Opening", path));
            }
            // SAFETY: `fd` is a freshly opened descriptor owned by nobody else.
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

    /// A validated private store root.
    #[derive(Clone, Debug)]
    pub(crate) struct LocalCaseStore {
        root: String,
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
        /// Open the store under an explicitly configured absolute root.
        pub(crate) fn open(root: &str) -> buck2_error::Result<Self> {
            Dir::open_root(root)?.dir(LAYOUT)?;
            Ok(Self {
                root: root.to_owned(),
            })
        }

        /// Take the exclusive per-key lease, waiting for another holder to finish. It spans the
        /// whole use of the key: a hit until the runner confirms or rejects it, or an attempt
        /// from its pending record through capture and publication. Same-key attempts therefore
        /// never overlap each other or a hit.
        pub(crate) fn lease(&self, key: &LocalCaseKey) -> buck2_error::Result<KeyGuard> {
            match self.acquire(key, true)? {
                Lease::Held(guard) => Ok(guard),
                Lease::Busy => Err(store_error("blocking lease returned busy".to_owned())),
            }
        }

        /// Take the lease only if no one holds it.
        #[cfg(test)]
        pub(crate) fn try_lease(&self, key: &LocalCaseKey) -> buck2_error::Result<Lease> {
            self.acquire(key, false)
        }

        fn acquire(&self, key: &LocalCaseKey, wait: bool) -> buck2_error::Result<Lease> {
            let layout = Dir::open_root(&self.root)?.dir(LAYOUT)?;
            let dir = layout.dir(&key.digest)?;
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
            let operation = if wait {
                libc::LOCK_EX
            } else {
                libc::LOCK_EX | libc::LOCK_NB
            };
            // SAFETY: `lock` is open for the lifetime of the guard.
            while unsafe { libc::flock(lock.as_raw_fd(), operation) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if !wait && e.raw_os_error() == Some(libc::EWOULDBLOCK) {
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
            }))
        }
    }

    /// The exclusive lease on one key, released when the last owner drops it.
    pub(crate) struct KeyGuard {
        key: LocalCaseKey,
        dir: Dir,
        generations: Dir,
        _lock: OwnedFd,
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
        pub(crate) fn lease(&self, _key: &LocalCaseKey) -> buck2_error::Result<KeyGuard> {
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
            panic!()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let store = store.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                let guard = store.lease(&key).unwrap();
                tx.send(()).unwrap();
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
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "the waiter overlapped the held hit");
        reused.reject("not confirmed").unwrap();
        assert_eq!(waiter.join().unwrap(), "miss");
        assert!(matches!(look(&store, &key), Seen::Miss(_)));
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
