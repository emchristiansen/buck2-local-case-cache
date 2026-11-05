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
