/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The fresh directories an `InternalRunnerTestInfo` declares with `fresh_dir_env`.
//!
//! A Testing request of such a provider gets one engine-owned scratch path, the executor's
//! existing per-request scratch: it is digested into the input tree as an empty directory, the
//! local executor removes and recreates it before every run and sets `TMPDIR` to it. The `i`th
//! declared name is a scratch directory variable of the request for the subdirectory `i` of it:
//! the local executor creates that directory empty and owner-only after preparing the scratch
//! path, and sets the variable to it after every other variable of the command, so no other
//! source of the environment replaces it. So, as with the external runner's `--fresh-dir-env`,
//! each name gets a new, empty directory of its own inside the command's `TMPDIR`, and listing
//! gets none.
//!
//! The scratch path is a function of the configured target and the request's suite, cases and
//! variant only: the same case gets the same absolute values, which the local case key records
//! byte for byte, and different cases never share one.
//!
//! By default it is a `buck-out` scratch path, whose absolute path leaves too little of the 107
//! usable bytes of an `AF_UNIX` address for a socket in it: its fixed part alone is 84 bytes, so a
//! tmux server's `tmux-<uid>/default` fits in the directory of name 0 only if the project root,
//! the isolation directory and the cell name come to at most 5 bytes together. When `[test] local_case_scratch_root` names a root R,
//! validated exactly like `[test] local_case_result_cache_root` (an absolute, canonical path of
//! real directories that no other principal can change, R itself private to this user), the
//! scratch path is instead R/<16 hex digits>, an owned scratch path of the request outside the
//! project: the local executor removes it and creates it again, owner-only, before every run, as
//! it does a `buck-out` one. Its digest also covers the absolute project root and `buck-out`
//! directory, so no two daemons share one, and the lease below remains the only exclusion. A
//! root of at most 70 bytes leaves room for `tmux-1000/default` in the directory of name 0. Requests that run the same case under
//! different keys do share it, so every execution of such a request holds the path's
//! [`ScratchLease`] from before its inputs are materialized (and the path removed) until its
//! child has terminated: every such execution runs as its own task with a real cancellation
//! context, so a cancelled or abandoned execution keeps the lease while the executor stops and
//! reaps its child. A tracked case keeps it until its result is published or rejected.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Weak;

use buck2_core::category::CategoryRef;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::buck_out_path::BuckOutScratchPath;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_execute::execute::request::CommandExecutionInput;
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_execute::execute::request::ScratchDirVariable;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_hash::BuckMutMap;
use buck2_test_api::data::TestStage;
use dupe::Dupe;

use crate::local_result_cache::KeyRecord;
use crate::local_result_cache::sha256;
use crate::local_result_cache::validate_scratch_root;

/// `[test] local_case_scratch_root`, validated as a root private to this user.
#[derive(Clone, Debug)]
pub(crate) struct ScratchRoot(String);

impl ScratchRoot {
    /// Validate the configured root, creating it owner-only if absent, as the store's root is.
    pub(crate) async fn validate(path: String) -> buck2_error::Result<Self> {
        let checked = path.clone();
        tokio::task::spawn_blocking(move || validate_scratch_root(&checked))
            .await
            .map_err(|e| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Environment,
                    "Validating [test] local_case_scratch_root did not complete: {e}"
                )
            })??;
        Ok(Self(path))
    }
}

/// Where a request's fresh directories are.
#[derive(Clone, Debug)]
enum Location {
    /// A `buck-out` scratch path, a scratch path input of the request.
    BuckOut(BuckOutScratchPath),
    /// A directory of a `ScratchRoot`, the request's owned scratch path.
    Owned,
}

/// The engine-owned scratch path of one Testing request and the directory each declared name
/// receives inside it.
#[derive(Clone, Debug)]
pub(crate) struct FreshDirs {
    location: Location,
    /// The scratch path, absolute, as the executor resolves it.
    absolute: AbsNormPathBuf,
    /// Each declared name, in declaration order, and its subdirectory of the scratch path.
    names: Vec<(String, ForwardRelativePathBuf)>,
}

impl FreshDirs {
    /// The fresh directories of a Testing request whose provider declares `names`, in `root` if
    /// one is configured and in `buck-out` otherwise; `None` for a listing or when no name is
    /// declared.
    pub(crate) fn for_stage<'a>(
        test_target: &ConfiguredProvidersLabel,
        stage: &TestStage,
        names: impl IntoIterator<Item = &'a str>,
        fs: &ArtifactFs,
        root: Option<&ScratchRoot>,
    ) -> buck2_error::Result<Option<Self>> {
        let TestStage::Testing {
            suite,
            testcases,
            variant,
            ..
        } = stage
        else {
            return Ok(None);
        };
        let names: Vec<(String, ForwardRelativePathBuf)> = names
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                Ok((
                    name.to_owned(),
                    ForwardRelativePathBuf::new(index.to_string())?,
                ))
            })
            .collect::<buck2_error::Result<_>>()?;
        if names.is_empty() {
            return Ok(None);
        }
        let mut record = KeyRecord::default();
        record.field("suite", suite.as_bytes());
        record.count("testcases", testcases.len());
        for case in testcases {
            record.field("case", case.as_bytes());
        }
        record.field("variant", variant.as_deref().unwrap_or_default().as_bytes());
        let (location, absolute) = match root {
            None => {
                // 128 bits of the digest: short enough to stay a readable path component.
                let identifier = &sha256(&record.0)[..32];
                let scratch = BuckOutScratchPath::new(
                    BaseDeferredKey::TargetLabel(test_target.target().dupe()),
                    CategoryRef::new("local_case_fresh")?,
                    Some(identifier),
                    ForwardRelativePathBuf::new("test".to_owned())?,
                    false,
                )?;
                let absolute = fs
                    .fs()
                    .resolve(&fs.buck_out_path_resolver().resolve_scratch(&scratch)?);
                (Location::BuckOut(scratch), absolute)
            }
            Some(ScratchRoot(root)) => {
                // The configured target, and the project and `buck-out` it is built in, which
                // `buck-out`'s own path would otherwise contribute.
                record.field("target", test_target.to_string().as_bytes());
                record.field(
                    "project",
                    fs.fs().root().as_path().as_os_str().as_encoded_bytes(),
                );
                record.field(
                    "buck_out",
                    fs.buck_out_path_resolver().root().as_str().as_bytes(),
                );
                // 64 bits of the digest, to keep socket paths in it short. A collision only
                // makes two cases share an exclusively leased path removed before each use.
                let identifier = &sha256(&record.0)[..16];
                let absolute =
                    AbsNormPathBuf::new(std::path::PathBuf::from(format!("{root}/{identifier}")))?;
                (Location::Owned, absolute)
            }
        };
        Ok(Some(Self {
            location,
            absolute,
            names,
        }))
    }

    /// The scratch path, resolved to an absolute path.
    pub(crate) fn absolute(&self) -> &AbsNormPathBuf {
        &self.absolute
    }

    /// Each declared name and the absolute path of its directory: the value the executor sets.
    #[cfg(test)]
    pub(crate) fn values(&self) -> buck2_error::Result<Vec<(String, String)>> {
        self.names
            .iter()
            .map(|(name, subdirectory)| {
                let path = self.absolute.join(subdirectory);
                let value = path.to_str()?.to_owned();
                Ok((name.clone(), value))
            })
            .collect()
    }

    /// The declared names.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(|(name, _)| name.as_str())
    }

    /// The scratch directory variables the executor sets, in declaration order.
    pub(crate) fn variables(&self) -> Vec<ScratchDirVariable> {
        self.names
            .iter()
            .map(|(name, subdirectory)| ScratchDirVariable {
                name: name.clone(),
                subdirectory: subdirectory.clone(),
            })
            .collect()
    }

    /// The request input that makes the executor prepare the scratch path.
    pub(crate) fn input(&self) -> Option<CommandExecutionInput> {
        match &self.location {
            Location::BuckOut(scratch) => Some(CommandExecutionInput::ScratchPath(scratch.clone())),
            Location::Owned => None,
        }
    }

    /// The request's owned scratch path, if the scratch path is not in `buck-out`.
    pub(crate) fn owned_scratch_path(&self) -> Option<AbsNormPathBuf> {
        match &self.location {
            Location::BuckOut(_) => None,
            Location::Owned => Some(self.absolute.clone()),
        }
    }

    /// Whether `request`'s scratch path, scratch inputs and scratch directory variables are
    /// exactly these, and it sets none of those variables itself, so that its scratch path is
    /// this engine-owned one and nothing else.
    pub(crate) fn admits(&self, request: &CommandExecutionRequest) -> bool {
        let mut scratch_inputs = request.inputs().iter().filter_map(|input| match input {
            CommandExecutionInput::ScratchPath(path) => Some(path),
            _ => None,
        });
        let first_scratch_input = scratch_inputs.next();
        let one_scratch = scratch_inputs.next().is_none()
            && match &self.location {
                Location::BuckOut(scratch) => {
                    first_scratch_input == Some(scratch) && request.owned_scratch_path().is_none()
                }
                Location::Owned => {
                    first_scratch_input.is_none()
                        && request.owned_scratch_path() == Some(&self.absolute)
                }
            };
        one_scratch
            && request.scratch_dir_variables() == self.variables().as_slice()
            && self.names().all(|name| !request.env().contains_key(name))
    }

    /// Wait for the exclusive use of the scratch path. It is released when dropped.
    pub(crate) async fn lease(&self) -> ScratchLease {
        let lock = {
            let mut active = ACTIVE_SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
            active.retain(|_, lock| lock.strong_count() > 0);
            match active.get(&self.absolute).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    active.insert(self.absolute.clone(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        ScratchLease {
            _guard: lock.lock_owned().await,
        }
    }
}

/// The scratch paths in use by this daemon. A daemon owns its `buck-out`, so no other process
/// uses them.
static ACTIVE_SCRATCH: LazyLock<
    std::sync::Mutex<BuckMutMap<AbsNormPathBuf, Weak<tokio::sync::Mutex<()>>>>,
> = LazyLock::new(Default::default);

/// Exclusive use of one fresh-directory scratch path by one execution.
pub(crate) struct ScratchLease {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl std::fmt::Debug for ScratchLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScratchLease")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use buck2_core::cells::CellResolver;
    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::cells::name::CellName;
    use buck2_core::configuration::data::ConfigurationData;
    use buck2_core::fs::buck_out_path::BuckOutPathResolver;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
    use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
    use buck2_execute::digest_config::DigestConfig;
    use buck2_execute::execute::request::CommandExecutionPaths;
    use buck2_hash::BuckFixedSortedMap;

    use super::*;

    fn artifact_fs(temp: &ProjectRootTemp) -> ArtifactFs {
        ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".into())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            temp.path().dupe(),
        )
    }

    fn target(name: &str) -> ConfiguredProvidersLabel {
        ConfiguredProvidersLabel::new(
            ConfiguredTargetLabel::testing_parse(
                &format!("cell//pkg:{name}"),
                ConfigurationData::testing_new(),
            ),
            Default::default(),
        )
    }

    fn testing(cases: &[&str], variant: Option<&str>) -> TestStage {
        TestStage::Testing {
            suite: "suite".to_owned(),
            testcases: cases.iter().map(|c| (*c).to_owned()).collect(),
            variant: variant.map(str::to_owned),
            repeat_count: None,
        }
    }

    fn fresh(
        fs: &ArtifactFs,
        target_name: &str,
        stage: &TestStage,
        names: &[&str],
    ) -> Option<FreshDirs> {
        FreshDirs::for_stage(&target(target_name), stage, names.iter().copied(), fs, None).unwrap()
    }

    // A scratch root exists only where the host store does (`validate_scratch_root`).
    #[cfg(unix)]
    fn owned(
        fs: &ArtifactFs,
        target_name: &str,
        stage: &TestStage,
        names: &[&str],
        root: &ScratchRoot,
    ) -> FreshDirs {
        FreshDirs::for_stage(
            &target(target_name),
            stage,
            names.iter().copied(),
            fs,
            Some(root),
        )
        .unwrap()
        .unwrap()
    }

    /// A new directory under the temporary directory, with a short name, removed when dropped.
    /// It is spelled through the temporary directory's canonical path, since a scratch root is
    /// validated like the result root, which refuses a symlinked ancestor such as macOS's `/var`.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let path = base.join(format!(
                "b2s{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_owned()
        }

        #[cfg(unix)]
        fn dir(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // A test may have made it unreadable to show it is refused.
                let _ignored =
                    std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
            let _ignored = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch_root(path: &str) -> buck2_error::Result<ScratchRoot> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(ScratchRoot::validate(path.to_owned()))
    }

    /// The request the orchestrator builds for `fresh`: its scratch input or owned scratch path
    /// and its scratch directory variables, plus an unrelated variable of its own.
    fn request_for(
        fs: &ArtifactFs,
        fresh: &FreshDirs,
        extra_inputs: Vec<CommandExecutionInput>,
    ) -> CommandExecutionRequest {
        let env: BuckFixedSortedMap<String, String> =
            [("OTHER".to_owned(), "1".to_owned())].into_iter().collect();
        let inputs = fresh.input().into_iter().chain(extra_inputs).collect();
        let paths = CommandExecutionPaths::new(
            inputs,
            Default::default(),
            fs,
            DigestConfig::testing_default(),
            None,
        )
        .unwrap();
        CommandExecutionRequest::new(vec![], vec!["test".to_owned()], paths, env)
            .with_scratch_dir_variables(fresh.variables())
            .with_owned_scratch_path(fresh.owned_scratch_path())
    }

    #[test]
    fn listing_and_undeclared_names_get_no_fresh_directories() {
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let listing = TestStage::Listing {
            suite: "suite".to_owned(),
            cacheable: true,
        };
        assert!(fresh(&fs, "t", &listing, &["FRESH_A"]).is_none());
        assert!(fresh(&fs, "t", &testing(&["a"], None), &[]).is_none());
    }

    /// Name `i` gets the absolute directory `<scratch>/<i>` inside the engine's scratch path; the
    /// path is a function of target, suite, cases and variant, the same for the same case and
    /// different for any other.
    #[test]
    fn each_name_gets_its_own_directory_inside_a_per_case_scratch_path() {
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let a = fresh(&fs, "t", &testing(&["a"], None), &["FRESH_A", "FRESH_B"]).unwrap();
        let scratch = a.absolute().to_string();
        assert!(
            scratch.starts_with(&temp.path().root().to_string()),
            "{scratch}"
        );
        // Each directory joins the scratch path with the host's own separator.
        let dir = |name: &str| {
            std::path::Path::new(&scratch)
                .join(name)
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            a.values().unwrap(),
            vec![
                ("FRESH_A".to_owned(), dir("0")),
                ("FRESH_B".to_owned(), dir("1")),
            ]
        );
        assert_eq!(
            a.variables(),
            vec![
                ScratchDirVariable {
                    name: "FRESH_A".to_owned(),
                    subdirectory: ForwardRelativePathBuf::new("0".to_owned()).unwrap(),
                },
                ScratchDirVariable {
                    name: "FRESH_B".to_owned(),
                    subdirectory: ForwardRelativePathBuf::new("1".to_owned()).unwrap(),
                },
            ]
        );

        let same = fresh(&fs, "t", &testing(&["a"], None), &["FRESH_A", "FRESH_B"]).unwrap();
        assert_eq!(same.absolute(), a.absolute());
        // The scratch path does not depend on the names, only their directories do.
        let renamed = fresh(&fs, "t", &testing(&["a"], None), &["OTHER"]).unwrap();
        assert_eq!(renamed.absolute(), a.absolute());

        let others = [
            fresh(&fs, "t", &testing(&["b"], None), &["FRESH_A"]).unwrap(),
            fresh(&fs, "t", &testing(&["a"], Some("v")), &["FRESH_A"]).unwrap(),
            fresh(&fs, "t", &testing(&["a", "b"], None), &["FRESH_A"]).unwrap(),
            fresh(&fs, "u", &testing(&["a"], None), &["FRESH_A"]).unwrap(),
        ];
        for (i, other) in others.iter().enumerate() {
            assert_ne!(other.absolute(), a.absolute(), "{i}");
            for (j, another) in others.iter().enumerate().skip(i + 1) {
                assert_ne!(other.absolute(), another.absolute(), "{i} {j}");
            }
        }
    }

    /// Admission accepts exactly the engine's own scratch path and scratch directory variables,
    /// with the request setting none of those names itself; any other scratch path, or the same
    /// one set up differently, is refused.
    #[test]
    fn admits_only_the_engines_own_scratch_setup() {
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let names = ["FRESH_A", "FRESH_B"];
        let a = fresh(&fs, "t", &testing(&["a"], None), &names).unwrap();
        let b = fresh(&fs, "t", &testing(&["b"], None), &names).unwrap();
        assert!(a.admits(&request_for(&fs, &a, vec![])));

        // Another case's scratch path, with variables naming its directories.
        assert!(!a.admits(&request_for(&fs, &b, vec![])));
        // A second scratch input.
        assert!(!a.admits(&request_for(&fs, &a, vec![b.input().unwrap()])));
        // No scratch input at all.
        let paths = CommandExecutionPaths::new(
            vec![],
            Default::default(),
            &fs,
            DigestConfig::testing_default(),
            None,
        )
        .unwrap();
        let bare = CommandExecutionRequest::new(
            vec![],
            vec!["test".to_owned()],
            paths,
            Default::default(),
        )
        .with_scratch_dir_variables(a.variables());
        assert!(!a.admits(&bare));
        // Missing, extra, renamed or reordered variables.
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_scratch_dir_variables(vec![])));
        let mut extra = a.variables();
        extra.push(ScratchDirVariable {
            name: "FRESH_C".to_owned(),
            subdirectory: ForwardRelativePathBuf::new("2".to_owned()).unwrap(),
        });
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_scratch_dir_variables(extra)));
        let mut renamed = a.variables();
        renamed[0].name = "OTHER_NAME".to_owned();
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_scratch_dir_variables(renamed)));
        let mut reordered = a.variables();
        reordered.reverse();
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_scratch_dir_variables(reordered)));
        // The request also setting a declared name itself.
        for name in names {
            let paths = CommandExecutionPaths::new(
                vec![a.input().unwrap()],
                Default::default(),
                &fs,
                DigestConfig::testing_default(),
                None,
            )
            .unwrap();
            let env: BuckFixedSortedMap<String, String> =
                [(name.to_owned(), "/elsewhere".to_owned())]
                    .into_iter()
                    .collect();
            let request = CommandExecutionRequest::new(vec![], vec!["test".to_owned()], paths, env)
                .with_scratch_dir_variables(a.variables());
            assert!(!a.admits(&request), "{name}");
        }
    }

    /// The lease of one scratch path is exclusive: a second holder waits until the first is
    /// dropped, whoever asks; a different path's lease does not wait.
    #[tokio::test]
    async fn the_scratch_lease_excludes_every_other_holder_of_the_same_path() {
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let a = fresh(&fs, "t", &testing(&["a"], None), &["FRESH_A"]).unwrap();
        // The same case, as another key (here, other names) would set it up.
        let a_again = fresh(&fs, "t", &testing(&["a"], None), &["OTHER"]).unwrap();
        let b = fresh(&fs, "t", &testing(&["b"], None), &["FRESH_A"]).unwrap();

        let first = a.lease().await;
        let short = Duration::from_millis(200);
        assert!(
            tokio::time::timeout(short, a_again.lease()).await.is_err(),
            "a second holder of the same scratch path did not wait"
        );
        let other = tokio::time::timeout(short, b.lease())
            .await
            .expect("another case's scratch path waited");

        let waiter = tokio::spawn(async move { a_again.lease().await });
        tokio::time::sleep(short).await;
        assert!(!waiter.is_finished(), "the waiter got the lease early");
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("the waiter never got the released lease")
            .unwrap();
        drop((second, other));
        // Released paths are forgotten once nobody holds or waits for them.
        let c = fresh(&fs, "t", &testing(&["c"], None), &["FRESH_A"]).unwrap();
        let _c = c.lease().await;
        let active = ACTIVE_SCRATCH.lock().unwrap();
        assert!(active.get(&a.absolute).is_none());
        assert!(active.get(&c.absolute).is_some());
    }

    /// With a scratch root, each case's scratch path is a 16-digit directory of the root, with
    /// the same layout inside; the same case of another project gets another one. A scratch root
    /// exists only on Linux and macOS, as the host store does.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_scratch_root_gives_each_case_of_each_project_a_short_directory_of_its_own() {
        let dir = TempDir::new();
        let root_path = dir.path("r");
        let root = scratch_root(&root_path).unwrap();
        let (one, two) = (
            ProjectRootTemp::new().unwrap(),
            ProjectRootTemp::new().unwrap(),
        );
        let (fs_one, fs_two) = (artifact_fs(&one), artifact_fs(&two));
        let names = ["FRESH_A", "FRESH_B"];
        let a = owned(&fs_one, "t", &testing(&["a"], None), &names, &root);

        let path = a.absolute().to_str().unwrap().to_owned();
        let identifier = path.strip_prefix(&format!("{root_path}/")).unwrap();
        assert_eq!(identifier.len(), 16, "{path}");
        assert!(identifier.bytes().all(|b| b.is_ascii_hexdigit()), "{path}");
        assert_eq!(
            a.values().unwrap(),
            vec![
                ("FRESH_A".to_owned(), format!("{path}/0")),
                ("FRESH_B".to_owned(), format!("{path}/1")),
            ]
        );
        assert!(a.input().is_none());
        assert_eq!(a.owned_scratch_path().as_ref(), Some(a.absolute()));

        // The same case of the same project again; another case; another project.
        let again = owned(&fs_one, "t", &testing(&["a"], None), &names, &root);
        assert_eq!(again.absolute(), a.absolute());
        let other_case = owned(&fs_one, "t", &testing(&["b"], None), &names, &root);
        assert_ne!(other_case.absolute(), a.absolute());
        let other_project = owned(&fs_two, "t", &testing(&["a"], None), &names, &root);
        assert_ne!(other_project.absolute(), a.absolute());
    }

    /// Admission accepts exactly the engine's own owned scratch path, with no scratch input.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn admits_only_the_engines_own_owned_scratch_path() {
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let dir = TempDir::new();
        let root = scratch_root(&dir.path("r")).unwrap();
        let names = ["FRESH_A"];
        let a = owned(&fs, "t", &testing(&["a"], None), &names, &root);
        let b = owned(&fs, "t", &testing(&["b"], None), &names, &root);
        let buck_out = fresh(&fs, "t", &testing(&["a"], None), &names).unwrap();
        assert!(a.admits(&request_for(&fs, &a, vec![])));

        // Another case's owned path; none; a `buck-out` scratch input beside it.
        assert!(!a.admits(&request_for(&fs, &b, vec![])));
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_owned_scratch_path(None)));
        assert!(!a.admits(&request_for(&fs, &a, vec![buck_out.input().unwrap()])));
        // The `buck-out` setup of the same case is not the owned one, nor the reverse.
        assert!(!a.admits(&request_for(&fs, &buck_out, vec![])));
        assert!(!buck_out.admits(&request_for(&fs, &a, vec![])));
        assert!(!buck_out.admits(
            &request_for(&fs, &buck_out, vec![]).with_owned_scratch_path(a.owned_scratch_path())
        ));
    }

    /// Elsewhere, as for the host store, any configured scratch root is refused.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn a_scratch_root_is_refused_where_the_host_store_is_unsupported() {
        let dir = TempDir::new();
        let Err(e) = scratch_root(&dir.path("r")) else {
            panic!("a scratch root was accepted");
        };
        let e = e.to_string();
        assert!(
            e.contains("[test] local_case_scratch_root is supported only on Linux and macOS"),
            "{e}"
        );
    }

    /// The scratch root is validated as the store's root is: absolute and canonical, every
    /// component a real directory, no ancestor another principal can change, and the root
    /// itself private. An absent root is created owner-only.
    #[cfg(unix)]
    #[test]
    fn the_scratch_root_is_validated_like_the_result_root() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let created = dir.path("created");
        scratch_root(&created).unwrap();
        let mode = std::fs::metadata(&created).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");

        let refused = |path: &str, expected: &str| {
            let e = format!("{:#}", scratch_root(path).unwrap_err());
            assert!(e.contains(expected), "{path}: {e}");
        };
        refused(
            "relative",
            "[test] local_case_scratch_root `relative` is not absolute",
        );
        refused(
            &format!("{created}/"),
            "has an empty component or a trailing slash",
        );

        let link = dir.path("link");
        std::os::unix::fs::symlink(&created, &link).unwrap();
        refused(&link, "symlinks are refused");
        refused(&format!("{link}/r"), "symlinks are refused");

        let shared = dir.path("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        refused(&format!("{shared}/r"), "Local case scratch root ancestor");
        assert!(!std::path::Path::new(&format!("{shared}/r")).exists());

        let open = dir.path("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o750)).unwrap();
        refused(&open, "Local case scratch root");
        refused(&open, "must be owned by uid");
    }

    /// The longest path an `AF_UNIX` address holds here: `sun_path` less its terminating NUL.
    #[cfg(unix)]
    fn af_unix_path_max() -> usize {
        // SAFETY: `sockaddr_un` is plain data, for which all zero bytes are a valid value.
        let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_path.len() - 1
    }

    /// The `std` refusal of an `AF_UNIX` path too long for an address.
    #[cfg(unix)]
    fn refused_as_too_long(result: std::io::Result<()>) {
        let e = result.unwrap_err();
        assert!(e.to_string().contains("path must be shorter than"), "{e}");
    }

    /// A Unix socket for a tmux server, `tmux-<uid>/default` in the directory of a fresh
    /// directory variable, cannot be bound in a `buck-out` scratch path, whose absolute path is
    /// too long for an `AF_UNIX` address, but can in a scratch root's of at most
    /// `af_unix_path_max()` less the 37 bytes the layout adds for uid 1000: 70 bytes on Linux.
    ///
    /// The test's own temporary directory can be longer than that (a remote worker's action
    /// directory is), so the limit is shown without it: the scratch path of a root of every
    /// length is the root and a fixed suffix; a socket path of exactly the limit binds in a real
    /// directory and one more byte is refused; so a socket path in a root at most that long
    /// binds. A root in the temporary directory is then bound for real when it is that short,
    /// and otherwise refused as too long, exactly as the limit says.
    #[cfg(unix)]
    #[test]
    fn a_tmux_socket_fits_in_a_scratch_root_but_not_in_buck_out() {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::net::UnixListener;

        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let max = af_unix_path_max();
        let socket = |fresh: &FreshDirs| {
            let (_, directory) = fresh.values().unwrap().remove(0);
            (
                format!("{directory}/tmux-{uid}"),
                format!("{directory}/tmux-{uid}/default"),
            )
        };
        let bind = |fresh: &FreshDirs| {
            let (server, socket) = socket(fresh);
            std::fs::create_dir_all(&server).unwrap();
            UnixListener::bind(socket).map(drop)
        };
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let stage = testing(&["a"], None);

        let buck_out = fresh(&fs, "t", &stage, &["TMUX_TMPDIR"]).unwrap();
        refused_as_too_long(bind(&buck_out));

        // What a scratch root adds to the socket path: `/<16 hex digits>/0/tmux-<uid>/default`.
        let added = format!("/{}/0/tmux-{uid}/default", "0".repeat(16)).len();
        let longest_root = max - added;
        #[cfg(target_os = "linux")]
        assert_eq!(
            max - "/0123456789abcdef/0/tmux-1000/default".len(),
            70,
            "the module documents a root of at most 70 bytes for uid 1000"
        );
        // The layout of roots one byte either side of the limit. It reads no directory, so these
        // roots need not exist; validation is `the_scratch_root_is_validated_like_the_result_root`.
        for length in [longest_root, longest_root + 1] {
            let root = ScratchRoot(format!("/{}", "r".repeat(length - 1)));
            let (_, path) = socket(&owned(&fs, "t", &stage, &["TMUX_TMPDIR"], &root));
            assert_eq!(path.len(), length + added, "{path}");
            if length > longest_root {
                refused_as_too_long(UnixListener::bind(&path).map(drop));
            }
        }

        // A socket path of exactly `max` bytes binds in a real directory, the temporary one,
        // whatever its length; on Linux the path names that directory through its descriptor.
        let dir = TempDir::new();
        let handle = std::fs::File::open(dir.dir()).unwrap();
        #[cfg(target_os = "linux")]
        let prefix = {
            use std::os::fd::AsRawFd;
            format!("/proc/self/fd/{}", handle.as_raw_fd())
        };
        #[cfg(not(target_os = "linux"))]
        let prefix = dir.dir().to_str().unwrap().to_owned();
        assert!(
            prefix.len() + 2 <= max,
            "no room for a socket under the {max}-byte AF_UNIX limit in {prefix}"
        );
        let name = "s".repeat(max - prefix.len() - 1);
        let at_limit = format!("{prefix}/{name}");
        assert_eq!(at_limit.len(), max);
        let listener = UnixListener::bind(&at_limit).unwrap();
        let bound = std::fs::symlink_metadata(dir.dir().join(&name)).unwrap();
        assert!(bound.file_type().is_socket());
        refused_as_too_long(UnixListener::bind(format!("{at_limit}s")).map(drop));
        drop((listener, handle));

        // A validated root in the temporary directory: bound if it is short enough, refused if not.
        let root_path = dir.path("r");
        let root = owned(
            &fs,
            "t",
            &stage,
            &["TMUX_TMPDIR"],
            &scratch_root(&root_path).unwrap(),
        );
        if root_path.len() <= longest_root {
            bind(&root).unwrap();
        } else {
            refused_as_too_long(bind(&root));
            eprintln!(
                "a_tmux_socket_fits_in_a_scratch_root_but_not_in_buck_out: the root {root_path} \
                 ({} bytes) is longer than {longest_root}, so its socket is refused; the \
                 limit itself was bound above",
                root_path.len()
            );
        }
    }
}
