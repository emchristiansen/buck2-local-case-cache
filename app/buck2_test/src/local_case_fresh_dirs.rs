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
//! declared name is set to the subdirectory `i` of it, which the local executor creates empty
//! after preparing the scratch path and before the command starts. So, as with the external
//! runner's `--fresh-dir-env`, each name gets a new, empty directory of its own inside the
//! command's `TMPDIR`, and listing gets none.
//!
//! The scratch path is a function of the configured target and the request's suite, cases and
//! variant only: the same case gets the same absolute values, which the local case key records
//! byte for byte, and different cases never share one. Requests that run the same case under
//! different keys do share it, so every execution of such a request holds the path's
//! [`ScratchLease`] from before its inputs are materialized (and the path removed) until it no
//! longer needs the path; a tracked case keeps it until its result is published or rejected.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Weak;

use buck2_core::category::CategoryRef;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::buck_out_path::BuckOutScratchPath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_execute::execute::request::CommandExecutionInput;
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_hash::BuckMutMap;
use buck2_test_api::data::TestStage;
use dupe::Dupe;

use crate::local_result_cache::KeyRecord;
use crate::local_result_cache::sha256;

/// The engine-owned scratch path of one Testing request and the directory each declared name
/// receives inside it.
#[derive(Clone, Debug)]
pub(crate) struct FreshDirs {
    scratch: BuckOutScratchPath,
    /// `scratch` resolved in the project.
    scratch_path: ProjectRelativePathBuf,
    /// `scratch` resolved to an absolute path, as the executor resolves it.
    absolute: AbsNormPathBuf,
    /// Each declared name, in declaration order, and its subdirectory of the scratch path.
    names: Vec<(String, ForwardRelativePathBuf)>,
}

impl FreshDirs {
    /// The fresh directories of a Testing request whose provider declares `names`; `None` for a
    /// listing or when no name is declared.
    pub(crate) fn for_stage<'a>(
        test_target: &ConfiguredProvidersLabel,
        stage: &TestStage,
        names: impl IntoIterator<Item = &'a str>,
        fs: &ArtifactFs,
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
        // 128 bits of the digest: short enough to stay a readable path component.
        let identifier = &sha256(&record.0)[..32];
        let scratch = BuckOutScratchPath::new(
            BaseDeferredKey::TargetLabel(test_target.target().dupe()),
            CategoryRef::new("local_case_fresh")?,
            Some(identifier),
            ForwardRelativePathBuf::new("test".to_owned())?,
            false,
        )?;
        let scratch_path = fs.buck_out_path_resolver().resolve_scratch(&scratch)?;
        let absolute = fs.fs().resolve(&scratch_path);
        Ok(Some(Self {
            scratch,
            scratch_path,
            absolute,
            names,
        }))
    }

    /// The scratch path, resolved to an absolute path.
    pub(crate) fn absolute(&self) -> &AbsNormPathBuf {
        &self.absolute
    }

    /// Each declared name and the absolute path of its directory.
    pub(crate) fn env(&self) -> buck2_error::Result<Vec<(String, String)>> {
        self.names
            .iter()
            .map(|(name, subdirectory)| {
                let path = self.absolute.join(subdirectory);
                let value = path.to_str()?.to_owned();
                Ok((name.clone(), value))
            })
            .collect()
    }

    /// The request input that makes the executor prepare the scratch path.
    pub(crate) fn input(&self) -> CommandExecutionInput {
        CommandExecutionInput::ScratchPath(self.scratch.clone())
    }

    /// The subdirectories the executor creates inside the scratch path.
    pub(crate) fn subdirectories(&self) -> Vec<ForwardRelativePathBuf> {
        self.names.iter().map(|(_, dir)| dir.clone()).collect()
    }

    /// Whether `request`'s scratch inputs, scratch subdirectories and declared variables are
    /// exactly these, so that its scratch path is this engine-owned one and nothing else.
    pub(crate) fn admits(&self, request: &CommandExecutionRequest) -> bool {
        let mut scratch_inputs = request.inputs().iter().filter_map(|input| match input {
            CommandExecutionInput::ScratchPath(path) => Some(path),
            _ => None,
        });
        let one_scratch = scratch_inputs.next() == Some(&self.scratch)
            && scratch_inputs.next().is_none();
        let Ok(env) = self.env() else {
            return false;
        };
        one_scratch
            && request.scratch_subdirectories() == self.subdirectories().as_slice()
            && env
                .iter()
                .all(|(name, value)| request.env().get(name) == Some(value))
    }

    /// Wait for the exclusive use of the scratch path. It is released when dropped.
    pub(crate) async fn lease(&self) -> ScratchLease {
        let lock = {
            let mut active = ACTIVE_SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
            active.retain(|_, lock| lock.strong_count() > 0);
            match active.get(&self.scratch_path).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    active.insert(self.scratch_path.clone(), Arc::downgrade(&lock));
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
    std::sync::Mutex<BuckMutMap<ProjectRelativePathBuf, Weak<tokio::sync::Mutex<()>>>>,
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
    use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
    use buck2_execute::digest_config::DigestConfig;
    use buck2_execute::execute::request::CommandExecutionPaths;
    use sorted_vector_map::SortedVectorMap;

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
        FreshDirs::for_stage(&target(target_name), stage, names.iter().copied(), fs).unwrap()
    }

    /// The request the orchestrator builds for `fresh`: its scratch input, its subdirectories and
    /// its variables, plus an unrelated variable.
    fn request_for(
        fs: &ArtifactFs,
        fresh: &FreshDirs,
        extra_inputs: Vec<CommandExecutionInput>,
    ) -> CommandExecutionRequest {
        let mut env: SortedVectorMap<String, String> = fresh.env().unwrap().into_iter().collect();
        env.insert("OTHER".to_owned(), "1".to_owned());
        let inputs = std::iter::once(fresh.input()).chain(extra_inputs).collect();
        let paths = CommandExecutionPaths::new(
            inputs,
            Default::default(),
            fs,
            DigestConfig::testing_default(),
            None,
        )
        .unwrap();
        CommandExecutionRequest::new(vec![], vec!["test".to_owned()], paths, env)
            .with_scratch_subdirectories(fresh.subdirectories())
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
        assert_eq!(
            a.env().unwrap(),
            vec![
                ("FRESH_A".to_owned(), format!("{scratch}/0")),
                ("FRESH_B".to_owned(), format!("{scratch}/1")),
            ]
        );
        assert_eq!(
            a.subdirectories(),
            vec![
                ForwardRelativePathBuf::new("0".to_owned()).unwrap(),
                ForwardRelativePathBuf::new("1".to_owned()).unwrap(),
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

    /// Admission accepts exactly the engine's own scratch path, subdirectories and values; any
    /// other scratch path, or the same one set up differently, is refused.
    #[test]
    fn admits_only_the_engines_own_scratch_setup() {
        let temp = ProjectRootTemp::new().unwrap();
        let fs = artifact_fs(&temp);
        let names = ["FRESH_A", "FRESH_B"];
        let a = fresh(&fs, "t", &testing(&["a"], None), &names).unwrap();
        let b = fresh(&fs, "t", &testing(&["b"], None), &names).unwrap();
        assert!(a.admits(&request_for(&fs, &a, vec![])));

        // Another case's scratch path, with values naming it.
        assert!(!a.admits(&request_for(&fs, &b, vec![])));
        // A second scratch input.
        assert!(!a.admits(&request_for(&fs, &a, vec![b.input()])));
        // No scratch input at all.
        let paths =
            CommandExecutionPaths::new(vec![], Default::default(), &fs, DigestConfig::testing_default(), None)
                .unwrap();
        let env: SortedVectorMap<String, String> = a.env().unwrap().into_iter().collect();
        let bare = CommandExecutionRequest::new(vec![], vec!["test".to_owned()], paths, env)
            .with_scratch_subdirectories(a.subdirectories());
        assert!(!a.admits(&bare));
        // Missing, extra or reordered subdirectories.
        let request = request_for(&fs, &a, vec![]);
        assert!(!a.admits(&request.with_scratch_subdirectories(vec![])));
        let mut extra = a.subdirectories();
        extra.push(ForwardRelativePathBuf::new("2".to_owned()).unwrap());
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_scratch_subdirectories(extra)));
        let mut reordered = a.subdirectories();
        reordered.reverse();
        assert!(!a.admits(&request_for(&fs, &a, vec![]).with_scratch_subdirectories(reordered)));
        // A value that names another directory, or a missing name.
        for (name, value) in [("FRESH_A", Some("/elsewhere".to_owned())), ("FRESH_B", None)] {
            let mut env: SortedVectorMap<String, String> = a.env().unwrap().into_iter().collect();
            match value {
                Some(value) => {
                    env.insert(name.to_owned(), value);
                }
                None => {
                    env.remove(name);
                }
            }
            let paths = CommandExecutionPaths::new(
                vec![a.input()],
                Default::default(),
                &fs,
                DigestConfig::testing_default(),
                None,
            )
            .unwrap();
            let request = CommandExecutionRequest::new(vec![], vec!["test".to_owned()], paths, env)
                .with_scratch_subdirectories(a.subdirectories());
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
        assert!(active.get(&a.scratch_path).is_none());
        assert!(active.get(&c.scratch_path).is_some());
    }
}
