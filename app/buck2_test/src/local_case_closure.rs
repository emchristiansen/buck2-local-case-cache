/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Which files a locally reused test case's command names, and whether each is bound.
//!
//! The prepared action digest binds the bytes of every file in the action's input tree, but
//! only the *names* of files the command reaches outside it. A case is reusable only if every
//! input-tree file, and every file its command names, is bound by one of the two existing
//! operands:
//!
//! - a build artifact in the action's input tree, under `buck-out`, whose bytes the digest binds
//!   and which only Buck2 writes; a checkout source file is not bound, since anyone may change
//!   it between keying, execution and publication, which digests taken before and after cannot
//!   rule out;
//! - a path in the Nix store, whose name binds its content and its whole runtime closure; a
//!   store path is accepted only if its canonical form stays in the store.
//!
//! So `argv[0]`, the `#!` interpreter of an input-tree script it launches, every `PATH` entry and
//! every other argument or environment value that names an existing file must be one of these,
//! and no loader override (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`) may be set. A literal host
//! tool such as `/bin/sh` is neither, so a case that names one is ineligible and publishes
//! nothing.
//!
//! The serialized input tree binds a symlink's target string, not its referent, so every
//! input-tree symlink, named or not, must resolve to a declared build artifact or a Nix store
//! path. Paths are resolved in kernel order: a symlink is replaced by its target before any later
//! component applies, and `..` is followed only between declared input-tree directories.
//!
//! This classifies operands; it does not discover dependencies. What a bound artifact itself
//! loads or reads while it runs is covered by the provider's explicit request, which declares
//! that its cases are deterministic within their declared inputs.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use buck2_common::file_ops::metadata::FileMetadata;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::directory_iterator::DirectoryIterator;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionImmutableDirectory;
use buck2_execute::materialize::materializer::MaterializationError;
use buck2_execute::materialize::materializer::MaterializationPurpose;
use buck2_execute::materialize::materializer::MaterializeRequest;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::materialize::materializer::ReadLease;
use buck2_hash::BuckMutMap;
use buck2_hash::BuckMutSet;
use dupe::Dupe;

/// Symlink hops followed while resolving one path, as the kernel's limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// The kernel reads at most this many bytes of a `#!` line.
const SHEBANG_LIMIT: usize = 256;

const LOADER_OVERRIDES: &[&str] = &["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT"];

/// One leaf of the action's input tree.
#[derive(Debug, Clone)]
enum Leaf {
    /// A file, as the exact value the action's input tree binds at its path.
    File(FileMetadata),
    /// A symlink whose target is relative to its own directory.
    Symlink(String),
    /// A symlink to an absolute host path.
    External(PathBuf),
}

/// The action's input tree, by project-relative path components.
#[derive(Debug)]
pub(crate) struct InputTree {
    leaves: BuckMutMap<Vec<String>, Leaf>,
    dirs: BuckMutSet<Vec<String>>,
}

/// Where a path ends after following input-tree symlinks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    Tree {
        path: Vec<String>,
        executable: bool,
    },
    TreeDir,
    Host(PathBuf),
    /// A project path the input tree does not contain.
    Undeclared(Vec<String>),
}

impl InputTree {
    pub(crate) fn new(directory: &ActionImmutableDirectory) -> Self {
        let mut leaves = BuckMutMap::default();
        let mut dirs = BuckMutSet::default();
        for (path, entry) in directory.unordered_walk().with_paths() {
            let components = path
                .iter()
                .map(|c| c.as_str().to_owned())
                .collect::<Vec<_>>();
            let leaf = match entry {
                DirectoryEntry::Dir(_) => {
                    dirs.insert(components);
                    continue;
                }
                DirectoryEntry::Leaf(ActionDirectoryMember::File(meta)) => Leaf::File(meta.dupe()),
                DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(link)) => {
                    Leaf::Symlink(link.target().as_str().to_owned())
                }
                DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(link)) => {
                    Leaf::External(link.to_path_buf())
                }
            };
            leaves.insert(components, leaf);
        }
        Self { leaves, dirs }
    }

    /// Resolve a project-relative path as the kernel would, one component at a time from the
    /// project root: an input-tree symlink is replaced by its target before any later component
    /// is applied, so a `..` applies to the directory the link resolved to, never lexically to
    /// the link. A `..` is followed only out of a declared input-tree directory into another
    /// one, since what lies on disk above or beside the declared tree, possibly behind a
    /// symlink the tree does not record, is unknown here.
    fn resolve(&self, path: &Path) -> Result<Resolved, String> {
        let mut pending = std::collections::VecDeque::new();
        push_components(&mut pending, path)
            .ok_or_else(|| format!("`{}` is not a relative path", path.display()))?;
        self.walk(Vec::new(), pending)
    }

    /// Continue resolving `pending` from the resolved, symlink-free directory `current`.
    fn walk(
        &self,
        mut current: Vec<String>,
        mut pending: std::collections::VecDeque<Step>,
    ) -> Result<Resolved, String> {
        let mut hops = 0;
        while let Some(step) = pending.pop_front() {
            let name = match step {
                Step::Parent => {
                    let parent = &current[..current.len().saturating_sub(1)];
                    if current.is_empty()
                        || !self.dirs.contains(&current)
                        || !self.dirs.contains(parent)
                    {
                        return Err(format!(
                            "`..` after `{}` leaves the declared input tree",
                            current.join("/")
                        ));
                    }
                    current.pop();
                    continue;
                }
                Step::Name(name) => name,
            };
            current.push(name);
            match self.leaves.get(&current) {
                None => {}
                Some(Leaf::File(meta)) => {
                    if pending.is_empty() {
                        return Ok(Resolved::Tree {
                            path: current,
                            executable: meta.is_executable,
                        });
                    }
                    // A file is not a directory: nothing below it exists.
                    current.extend(pending.into_iter().map(Step::into_name));
                    return Ok(Resolved::Undeclared(current));
                }
                Some(Leaf::External(target)) => {
                    let mut host = target.clone();
                    host.extend(pending.into_iter().map(Step::into_name));
                    return Ok(Resolved::Host(host));
                }
                Some(Leaf::Symlink(target)) => {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(format!(
                            "`{}` passes more than {MAX_SYMLINK_HOPS} input-tree symlinks",
                            current.join("/")
                        ));
                    }
                    let link = current.join("/");
                    current.pop();
                    let mut next = std::collections::VecDeque::new();
                    push_components(&mut next, Path::new(target)).ok_or_else(|| {
                        format!("input-tree symlink `{link}` -> `{target}` is not relative")
                    })?;
                    next.extend(pending);
                    pending = next;
                }
            }
        }
        // The project root itself is not a declared input-tree directory.
        Ok(if self.dirs.contains(&current) {
            Resolved::TreeDir
        } else {
            Resolved::Undeclared(current)
        })
    }
}

/// One component of a relative path still to be resolved.
#[derive(Debug, Clone)]
enum Step {
    Name(String),
    Parent,
}

impl Step {
    fn into_name(self) -> String {
        match self {
            Self::Name(name) => name,
            Self::Parent => "..".to_owned(),
        }
    }
}

/// Append the components of a relative path, keeping `..` for resolution in kernel order and
/// dropping `.`; `None` if the path is not relative or not UTF-8.
fn push_components(steps: &mut std::collections::VecDeque<Step>, path: &Path) -> Option<()> {
    for component in path.components() {
        match component {
            Component::Normal(c) => steps.push_back(Step::Name(c.to_str()?.to_owned())),
            Component::CurDir => {}
            Component::ParentDir => steps.push_back(Step::Parent),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(())
}

/// A plain absolute path inside the Nix store whose canonical form stays there, which therefore
/// names immutable content. Returns the canonical path.
fn store_path(path: &Path) -> Result<PathBuf, String> {
    fn in_store(path: &Path) -> bool {
        let mut components = path.components();
        components.next() == Some(Component::RootDir)
            && components.next() == Some(Component::Normal(OsStr::new("nix")))
            && components.next() == Some(Component::Normal(OsStr::new("store")))
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.all(|c| matches!(c, Component::Normal(_)))
    }
    if !in_store(path) {
        return Err(format!("`{}` is not a Nix store path", path.display()));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("`{}` cannot be resolved: {e}", path.display()))?;
    if !in_store(&canonical) {
        return Err(format!(
            "`{}` resolves to `{}` outside the Nix store",
            path.display(),
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// A file the command names, once found bound.
enum Bound {
    /// An input-tree build artifact.
    Artifact {
        path: Vec<String>,
        executable: bool,
    },
    Store,
}

/// The facts the plan is decided from.
pub(crate) struct LaunchFacts<'a> {
    pub(crate) project_root: &'a Path,
    /// Project-relative components of the child's working directory.
    pub(crate) working_directory: Vec<String>,
    /// Project-relative components of `buck-out`.
    pub(crate) buck_out: Vec<String>,
    pub(crate) argv: &'a [String],
    /// The child's whole environment as installed.
    pub(crate) env: &'a [(OsString, OsString)],
}

/// The input-tree executables the command launches, whose `#!` lines must be read from the
/// exact values the action's input tree binds at their paths. Only [`LaunchPlan::materialize`]
/// gives a plan that can be verified: there is no way to read a launched executable's header
/// from whatever happens to occupy its path.
#[derive(Debug, Clone)]
pub(crate) struct LaunchPlan {
    project_root: PathBuf,
    executables: Vec<Launched>,
}

/// One launched input-tree executable: its path and the file value the action's input tree, and
/// so the action digest and the case's key, binds there. The local executor materializes this
/// same value at this path before it starts the command.
#[derive(Debug, Clone)]
struct Launched {
    path: Vec<String>,
    value: FileMetadata,
}

impl Launched {
    fn project_path(&self) -> ProjectRelativePathBuf {
        ProjectRelativePathBuf::unchecked_new(self.path.join("/"))
    }
}

/// A launch plan whose executables were materialized as their requested values and are held
/// for reading by `_lease` for as long as this lives. [`MaterializedLaunch::verify`] is the only
/// launch check there is, so the check always reads the bytes of the requested values.
///
/// It is held over the check and released before the action is executed: the executor then
/// requests these same values at these same paths, under its own lease, so what it starts is
/// what was checked. Keeping this lease while the executor acquires its own would be an
/// acquisition made while holding, which the disk-state core's lock ordering rules out.
#[must_use = "verify the launch while the lease holds the executables"]
pub(crate) struct MaterializedLaunch {
    plan: LaunchPlan,
    _lease: ReadLease,
}

impl LaunchPlan {
    /// Classify every file the request's command names, or say which one is not bound.
    pub(crate) fn new(tree: &InputTree, facts: LaunchFacts<'_>) -> Result<Self, String> {
        if let Some(source) = tree
            .leaves
            .keys()
            .filter(|path| !path.starts_with(&facts.buck_out))
            .min()
        {
            return Err(format!(
                "input `{}` is a checkout file, not a build artifact under buck-out",
                source.join("/")
            ));
        }

        // Every input-tree symlink, whether or not the command names it, must reach immutable
        // bound content: a declared build artifact, or a path in the Nix store. The serialized
        // tree binds only a symlink's target string, so any other referent, such as a host file
        // or an undeclared project file or directory, could change without changing the key.
        let mut links = tree
            .leaves
            .iter()
            .filter(|(_, leaf)| !matches!(leaf, Leaf::File(..)))
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        links.sort();
        for link in links {
            let name = link.join("/");
            let steps = link.iter().map(|c| Step::Name(c.clone())).collect();
            match tree
                .walk(Vec::new(), steps)
                .map_err(|e| format!("input-tree symlink `{name}`: {e}"))?
            {
                Resolved::Tree { .. } => {}
                Resolved::Host(host) => {
                    store_path(&host).map_err(|e| format!("input-tree symlink `{name}`: {e}"))?;
                }
                Resolved::TreeDir => {
                    return Err(format!(
                        "input-tree symlink `{name}` names a directory, not a declared artifact"
                    ));
                }
                Resolved::Undeclared(path) => {
                    return Err(format!(
                        "input-tree symlink `{name}` resolves to `{}`, which is not a declared \
                         artifact",
                        path.join("/")
                    ));
                }
            }
        }

        let mut path_var = None;
        for (name, value) in facts.env {
            if let Some(name) = name.to_str() {
                if LOADER_OVERRIDES.contains(&name) {
                    return Err(format!("the environment sets the loader override {name}"));
                }
                if name == "PATH" {
                    path_var = Some(value);
                }
            }
        }
        let path_var =
            path_var.ok_or("PATH is unset, so lookups use the host's default search path")?;
        for dir in std::env::split_paths(path_var) {
            store_path(&dir).map_err(|e| format!("PATH entry {e}"))?;
        }

        let classify = Classify {
            tree,
            facts: &facts,
        };
        let mut executables: Vec<Vec<String>> = Vec::new();
        let (argv0, args) = facts.argv.split_first().ok_or("the command is empty")?;
        if !argv0.contains('/') {
            return Err(format!(
                "argv[0] `{argv0}` is found through the working directory or PATH"
            ));
        }
        match classify.named(argv0)? {
            Some(Bound::Artifact {
                path,
                executable: true,
            }) => executables.push(path),
            Some(Bound::Artifact { .. }) => {
                return Err(format!("argv[0] `{argv0}` is not executable"));
            }
            Some(Bound::Store) => {}
            None => return Err(format!("argv[0] `{argv0}` does not exist")),
        }

        let values = args
            .iter()
            .map(|a| OsStr::new(a.as_str()))
            .chain(facts.env.iter().map(|(_, v)| v.as_os_str()));
        for value in values {
            let Some(value) = value.to_str() else {
                continue;
            };
            let after_eq = value.split_once('=').map(|(_, v)| v);
            for candidate in std::iter::once(value).chain(after_eq) {
                if candidate.is_empty() || candidate.contains('\0') {
                    continue;
                }
                if let Some(Bound::Artifact {
                    path,
                    executable: true,
                }) = classify.named(candidate)?
                {
                    if !executables.contains(&path) {
                        executables.push(path);
                    }
                }
            }
        }
        let executables = executables
            .into_iter()
            .map(|path| match tree.leaves.get(&path) {
                Some(Leaf::File(value)) => Ok(Launched {
                    value: value.dupe(),
                    path,
                }),
                _ => Err(format!(
                    "launched artifact `{}` is not a file of the input tree",
                    path.join("/")
                )),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            project_root: facts.project_root.to_owned(),
            executables,
        })
    }

    /// The input-tree executables, each with the exact value the input tree binds at its path.
    pub(crate) fn launched_values(&self) -> Vec<(ProjectRelativePathBuf, ArtifactValue)> {
        self.executables
            .iter()
            .map(|launched| {
                (
                    launched.project_path(),
                    ArtifactValue::file(launched.value.dupe()),
                )
            })
            .collect()
    }

    /// Put the requested value of every launched executable at its path and hold it for
    /// reading. A path-only request is not enough: a materializer that tracks disk state has no
    /// value to make for a bare path and leaves whatever is there, which may be nothing or an
    /// older artifact's bytes.
    pub(crate) async fn materialize(
        self,
        materializer: &dyn Materializer,
        re_use_case: RemoteExecutorUseCase,
    ) -> buck2_error::Result<Result<MaterializedLaunch, MaterializationError>> {
        if self.executables.is_empty() {
            return Ok(Ok(MaterializedLaunch {
                plan: self,
                _lease: ReadLease::noop(),
            }));
        }
        let response = materializer
            .materialize(MaterializeRequest {
                artifacts: self.launched_values(),
                outputs: Vec::new(),
                purpose: MaterializationPurpose::IntermediateOnly,
                re_use_case,
            })
            .await?;
        Ok(response
            .ensure_results_ok()
            .map(|lease| MaterializedLaunch {
                plan: self,
                _lease: lease,
            }))
    }

    /// This plan as if its values were materialized, for tests of the header check alone, which
    /// put the requested bytes on disk themselves. The orchestrator's composed tests cover how a
    /// real materializer puts them there.
    #[cfg(all(test, unix))]
    pub(crate) fn testing_held(&self) -> MaterializedLaunch {
        MaterializedLaunch {
            plan: self.clone(),
            _lease: ReadLease::noop(),
        }
    }
}

impl MaterializedLaunch {
    /// Check that each launched input-tree script's `#!` interpreter is a Nix store path, reading
    /// the requested values this holds. A file whose length is not its requested value's is
    /// refused rather than read as if it were that value.
    pub(crate) fn verify(&self) -> Result<(), String> {
        for launched in &self.plan.executables {
            let path = &launched.path;
            let mut disk = self.plan.project_root.clone();
            disk.extend(path);
            let mut head = Vec::with_capacity(SHEBANG_LIMIT);
            {
                use std::io::Read;
                let file = std::fs::File::open(&disk)
                    .map_err(|e| format!("`{}` cannot be read: {e}", disk.display()))?;
                let len = file
                    .metadata()
                    .map_err(|e| format!("`{}` cannot be read: {e}", disk.display()))?
                    .len();
                let expected = launched.value.digest.size();
                if len != expected {
                    return Err(format!(
                        "`{}` holds {len} bytes, not the {expected} bytes of its requested value",
                        disk.display()
                    ));
                }
                file.take(SHEBANG_LIMIT as u64)
                    .read_to_end(&mut head)
                    .map_err(|e| format!("`{}` cannot be read: {e}", disk.display()))?;
            }
            let Some(line) = head.strip_prefix(b"#!") else {
                continue;
            };
            let line = line.split(|&b| b == b'\n').next().unwrap_or_default();
            let interpreter = line
                .split(|&b| b == b' ' || b == b'\t')
                .find(|token| !token.is_empty())
                .and_then(|token| std::str::from_utf8(token).ok())
                .ok_or_else(|| format!("`{}` has no readable `#!` interpreter", disk.display()))?;
            store_path(Path::new(interpreter))
                .map_err(|e| format!("interpreter of `{}`: {e}", path.join("/")))?;
        }
        Ok(())
    }
}

struct Classify<'a> {
    tree: &'a InputTree,
    facts: &'a LaunchFacts<'a>,
}

impl Classify<'_> {
    /// Classify a value that may name a file: `Ok(None)` if it names nothing that exists, an
    /// error if it names an existing file that is not bound.
    fn named(&self, value: &str) -> Result<Option<Bound>, String> {
        let path = Path::new(value);
        let resolved = if path.is_absolute() {
            match path.strip_prefix(self.facts.project_root) {
                Ok(inside) => self.tree.resolve(inside),
                Err(_) => return self.host(path, value),
            }
        } else {
            // From the project root through the working directory, as the kernel resolved it.
            let mut steps = self
                .facts
                .working_directory
                .iter()
                .map(|c| Step::Name(c.clone()))
                .collect();
            push_components(&mut steps, path)
                .ok_or_else(|| format!("`{value}` is not a plain relative path"))?;
            self.tree.walk(Vec::new(), steps)
        }
        .map_err(|e| format!("`{value}`: {e}"))?;
        match resolved {
            Resolved::Tree { path, executable } => Ok(Some(Bound::Artifact { path, executable })),
            Resolved::TreeDir => Ok(None),
            Resolved::Host(host) => self.host(&host, value),
            Resolved::Undeclared(path) => {
                let mut disk = self.facts.project_root.to_owned();
                disk.extend(&path);
                match std::fs::metadata(&disk) {
                    Ok(meta) if meta.is_file() => Err(format!(
                        "`{value}` names the undeclared project file `{}`",
                        path.join("/")
                    )),
                    _ => Ok(None),
                }
            }
        }
    }

    fn host(&self, path: &Path, value: &str) -> Result<Option<Bound>, String> {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                store_path(path).map_err(|e| format!("`{value}` names a host file: {e}"))?;
                Ok(Some(Bound::Store))
            }
            _ => Ok(None),
        }
    }
}

// Every test here launches against the declared shell, a Nix store path (`crate::testing`), which
// only Unix hosts supply; the host-local case store that uses this check is Linux and macOS only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A file leaf holding `content`.
    fn file_leaf(content: &[u8], executable: bool) -> Leaf {
        use buck2_common::file_ops::metadata::TrackedFileDigest;
        use buck2_execute::digest_config::DigestConfig;
        Leaf::File(FileMetadata {
            digest: TrackedFileDigest::from_content(
                content,
                DigestConfig::testing_default().cas_digest_config(),
            ),
            is_executable: executable,
        })
    }

    fn tree(leaves: &[(&str, Leaf)], dirs: &[&str]) -> InputTree {
        let split = |p: &str| p.split('/').map(str::to_owned).collect::<Vec<_>>();
        let leaves: BuckMutMap<_, _> = leaves.iter().map(|(p, l)| (split(p), l.clone())).collect();
        // Every ancestor of a leaf is a directory of the tree, as in a real input directory.
        let mut all_dirs: BuckMutSet<Vec<String>> = dirs.iter().map(|d| split(d)).collect();
        for path in leaves.keys() {
            for end in 1..path.len() {
                all_dirs.insert(path[..end].to_vec());
            }
        }
        InputTree {
            leaves,
            dirs: all_dirs,
        }
    }

    fn store_dir() -> String {
        // The declared shell's store directory, never a host path (`crate::testing`).
        crate::testing::declared_shell_dir()
    }

    fn plan(
        tree: &InputTree,
        root: &Path,
        argv: &[&str],
        env: &[(&str, &str)],
    ) -> Result<LaunchPlan, String> {
        let argv = argv.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let env = env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect::<Vec<_>>();
        LaunchPlan::new(
            tree,
            LaunchFacts {
                project_root: root,
                working_directory: vec![],
                buck_out: vec!["buck-out".into(), "v2".into()],
                argv: &argv,
                env: &env,
            },
        )
    }

    #[test]
    fn literal_host_tools_checkout_and_undeclared_files_are_not_bound() {
        let root = tempfile_root("closure-literal");
        let store = store_dir();
        let sh = format!("{store}/sh");
        let script = "buck-out/v2/gen/t/script";
        let path_env = ("PATH", store.as_str());
        let t = tree(
            &[(script, file_leaf(b"artifact", true))],
            &[
                "buck-out",
                "buck-out/v2",
                "buck-out/v2/gen",
                "buck-out/v2/gen/t",
            ],
        );

        // A literal host tool: an existing file outside the project and the store.
        let host_sh = crate::testing::non_store_shell(&root.with_extension("host"));
        let host_sh = host_sh.to_str().unwrap();
        let e = plan(&t, &root, &[host_sh, script], &[path_env]).unwrap_err();
        assert!(e.contains(host_sh), "{e}");
        assert!(plan(&t, &root, &[&sh, script], &[path_env]).is_ok());
        let e = plan(&t, &root, &["sh", script], &[path_env]).unwrap_err();
        assert!(e.contains("PATH"), "{e}");
        let e = plan(&t, &root, &[&sh, script], &[("PATH", "/usr/bin")]).unwrap_err();
        assert!(e.contains("PATH entry"), "{e}");
        let e = plan(&t, &root, &[&sh, script], &[]).unwrap_err();
        assert!(e.contains("PATH is unset"), "{e}");
        let e = plan(&t, &root, &[&sh, script], &[path_env, ("LD_PRELOAD", "x")]).unwrap_err();
        assert!(e.contains("LD_PRELOAD"), "{e}");
        let e = plan(&t, &root, &[&sh, script, "/etc/hostname"], &[path_env]).unwrap_err();
        assert!(e.contains("/etc/hostname"), "{e}");
        let e = plan(
            &t,
            &root,
            &[&sh, script],
            &[path_env, ("F", "x=/etc/hostname")],
        )
        .unwrap_err();
        assert!(e.contains("/etc/hostname"), "{e}");
        // Values naming nothing that exists are not file operands.
        assert!(
            plan(
                &t,
                &root,
                &[&sh, script, "case_a", "/no/such/file"],
                &[path_env]
            )
            .is_ok()
        );

        std::fs::write(root.join("undeclared.txt"), "live").unwrap();
        let e = plan(&t, &root, &[&sh, script, "undeclared.txt"], &[path_env]).unwrap_err();
        assert!(e.contains("undeclared project file"), "{e}");

        // A declared checkout file is an input, but no build artifact.
        let with_source = tree(
            &[
                (script, file_leaf(b"artifact", true)),
                ("src/lib.rs", file_leaf(b"artifact", false)),
            ],
            &[],
        );
        let e = plan(&with_source, &root, &[&sh, script], &[path_env]).unwrap_err();
        assert!(e.contains("`src/lib.rs` is a checkout file"), "{e}");
    }

    #[test]
    fn launched_artifacts_need_store_interpreters() {
        let root = tempfile_root("closure-shebang");
        let store = store_dir();
        let path_env = ("PATH", store.as_str());
        let dir = root.join("buck-out/v2/gen/t");
        std::fs::create_dir_all(&dir).unwrap();
        let link_target = "../t/script";
        // The tree binds `content` as the script's value; the check reads it from disk, where a
        // materializer put it.
        let check = |content: &[u8]| {
            let t = tree(
                &[
                    ("buck-out/v2/gen/t/script", file_leaf(content, true)),
                    (
                        "buck-out/v2/gen/u/link",
                        Leaf::Symlink(link_target.to_owned()),
                    ),
                    (
                        "buck-out/v2/gen/t/tool",
                        Leaf::External(PathBuf::from(format!("{store}/sh"))),
                    ),
                ],
                &[],
            );
            std::fs::write(dir.join("script"), content).unwrap();
            let p = plan(&t, &root, &["buck-out/v2/gen/u/link"], &[path_env]).unwrap();
            let values = p.launched_values();
            assert_eq!(values.len(), 1);
            assert_eq!(
                values[0].0,
                ProjectRelativePathBuf::unchecked_new("buck-out/v2/gen/t/script".to_owned())
            );
            // The request is the tree's own value for the script.
            let DirectoryEntry::Leaf(ActionDirectoryMember::File(meta)) = values[0].1.entry()
            else {
                panic!("a launched executable is requested as a file");
            };
            let Some(Leaf::File(bound)) = t.leaves.get(
                ["buck-out", "v2", "gen", "t", "script"]
                    .map(str::to_owned)
                    .as_slice(),
            ) else {
                panic!("the script is a file of the tree");
            };
            assert_eq!(meta, bound);
            (t, p.testing_held().verify())
        };

        let e = check(b"#!/bin/sh\necho\n").1.unwrap_err();
        assert!(e.contains("/bin/sh"), "{e}");
        check(format!("#!{store}/sh -e\necho\n").as_bytes())
            .1
            .unwrap();
        let (t, ok) = check(b"\x7fELF binary");
        ok.unwrap();

        // Bytes of another length than the requested value's are not read as that value.
        std::fs::write(dir.join("script"), b"\x7fELF other binary").unwrap();
        let p = plan(&t, &root, &["buck-out/v2/gen/u/link"], &[path_env]).unwrap();
        let e = p.testing_held().verify().unwrap_err();
        assert!(e.contains("requested value"), "{e}");

        // An external symlink into the store is a bound tool.
        let p = plan(&t, &root, &["buck-out/v2/gen/t/tool"], &[path_env]).unwrap();
        assert!(p.launched_values().is_empty());
    }

    /// An input tree built the way an action's input directory is: leaves inserted into an
    /// `ActionDirectoryBuilder` and fingerprinted with the real directory serializer.
    fn real_tree(leaves: Vec<(&str, ActionDirectoryMember)>) -> InputTree {
        use buck2_directory::directory::entry::DirectoryEntry;
        use buck2_execute::digest_config::DigestConfig;
        use buck2_execute::directory::ActionDirectoryBuilder;
        use buck2_execute::directory::insert_entry;

        let digest_config = DigestConfig::testing_default();
        let mut builder = ActionDirectoryBuilder::empty_non_exhaustive();
        for (path, member) in leaves {
            insert_entry(
                &mut builder,
                ProjectRelativePathBuf::unchecked_new(path.to_owned()),
                DirectoryEntry::Leaf(member),
            )
            .unwrap();
        }
        InputTree::new(&builder.fingerprint(digest_config.as_directory_serializer()))
    }

    fn artifact(executable: bool) -> ActionDirectoryMember {
        use buck2_common::file_ops::metadata::FileMetadata;
        use buck2_common::file_ops::metadata::TrackedFileDigest;
        use buck2_execute::digest_config::DigestConfig;
        ActionDirectoryMember::File(FileMetadata {
            digest: TrackedFileDigest::from_content(
                b"artifact",
                DigestConfig::testing_default().cas_digest_config(),
            ),
            is_executable: executable,
        })
    }

    fn relative_link(target: &str) -> ActionDirectoryMember {
        use buck2_common::file_ops::metadata::Symlink;
        ActionDirectoryMember::Symlink(std::sync::Arc::new(Symlink::new(target.into())))
    }

    fn external_link(target: &Path) -> ActionDirectoryMember {
        use buck2_common::external_symlink::ExternalSymlink;
        use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
        ActionDirectoryMember::ExternalSymlink(std::sync::Arc::new(
            ExternalSymlink::new(target.to_owned(), ForwardRelativePathBuf::default()).unwrap(),
        ))
    }

    #[test]
    fn every_input_tree_symlink_must_reach_a_declared_artifact_or_the_store() {
        let root = tempfile_root("closure-links");
        let store = store_dir();
        let sh = format!("{store}/sh");
        let path_env = ("PATH", store.as_str());
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("payload"), "mutable").unwrap();
        std::fs::create_dir_all(root.join("undeclared")).unwrap();
        std::fs::write(root.join("undeclared/payload"), "mutable").unwrap();
        let script = "buck-out/v2/gen/t/script";
        // Only the containing resource layout is named on the command line, never a link.
        let argv = [sh.as_str(), script, "buck-out/v2/gen/t/resources"];
        let with = |extra: Vec<(&str, ActionDirectoryMember)>| {
            let mut leaves = vec![
                (script, artifact(true)),
                ("buck-out/v2/gen/t/resources/data", artifact(false)),
            ];
            leaves.extend(extra);
            plan(&real_tree(leaves), &root, &argv, &[path_env])
        };

        // Positive: a relative link to a declared artifact and an external link into the store.
        with(vec![
            ("buck-out/v2/gen/t/resources/alias", relative_link("data")),
            (
                "buck-out/v2/gen/t/resources/tool",
                external_link(Path::new(&sh)),
            ),
        ])
        .unwrap();

        // An external link to a mutable host file.
        let e = with(vec![(
            "buck-out/v2/gen/t/resources/hidden",
            external_link(&outside.join("payload")),
        )])
        .unwrap_err();
        assert!(e.contains("resources/hidden"), "{e}");
        // A relative chain ending at an undeclared project file.
        let e = with(vec![
            ("buck-out/v2/gen/t/resources/a", relative_link("b")),
            (
                "buck-out/v2/gen/t/resources/b",
                relative_link("../../../../../undeclared/payload"),
            ),
        ])
        .unwrap_err();
        assert!(e.contains("resources/a"), "{e}");
        // A link to a directory, whose undeclared contents it would expose.
        let e = with(vec![(
            "buck-out/v2/gen/t/resources/dir",
            relative_link("../../t"),
        )])
        .unwrap_err();
        assert!(e.contains("names a directory"), "{e}");
        // A dangling link.
        let e = with(vec![(
            "buck-out/v2/gen/t/resources/dangling",
            relative_link("missing"),
        )])
        .unwrap_err();
        assert!(e.contains("resources/dangling"), "{e}");
    }

    #[test]
    fn parent_components_resolve_after_symlinks_as_the_kernel_does() {
        use std::os::unix::fs::symlink;
        let root = tempfile_root("closure-dotdot");
        let store = store_dir();
        let sh = format!("{store}/sh");
        let path_env = ("PATH", store.as_str());
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        std::fs::write(outside.join("payload"), "outside").unwrap();
        // On disk exactly as the tree describes it, plus an undeclared directory symlink.
        let v2 = root.join("buck-out/v2");
        std::fs::create_dir_all(v2.join("gen/a")).unwrap();
        std::fs::write(v2.join("payload"), "declared").unwrap();
        std::fs::write(v2.join("gen/b"), "declared").unwrap();
        let _ignored = std::fs::remove_file(v2.join("link"));
        symlink(outside.join("deep"), v2.join("link")).unwrap();
        let _ignored = std::fs::remove_file(v2.join("undeclared"));
        symlink(outside.join("deep"), v2.join("undeclared")).unwrap();
        let _ignored = std::fs::remove_file(v2.join("chain"));
        symlink("link/../payload", v2.join("chain")).unwrap();
        let t = tree(
            &[
                ("buck-out/v2/payload", file_leaf(b"artifact", false)),
                ("buck-out/v2/gen/b", file_leaf(b"artifact", false)),
                ("buck-out/v2/link", Leaf::External(outside.join("deep"))),
                (
                    "buck-out/v2/chain",
                    Leaf::Symlink("link/../payload".to_owned()),
                ),
            ],
            &["buck-out/v2/gen/a"],
        );
        let kernel = |value: &str| std::fs::canonicalize(root.join(value)).unwrap();
        let resolved = |value: &str| match t.resolve(Path::new(value)).unwrap() {
            Resolved::Tree { path, .. } => {
                std::fs::canonicalize(root.join(path.join("/"))).unwrap()
            }
            Resolved::Host(host) => std::fs::canonicalize(host).unwrap(),
            other => panic!("{value}: {other:?}"),
        };

        // The counterexample: `link/..` is the parent of the link's referent, not `buck-out/v2`.
        let value = "buck-out/v2/link/../payload";
        assert_eq!(
            kernel(value),
            outside.join("payload").canonicalize().unwrap()
        );
        assert_ne!(
            t.resolve(Path::new(value)).unwrap(),
            Resolved::Tree {
                path: vec!["buck-out".into(), "v2".into(), "payload".into()],
                executable: false
            }
        );
        assert_eq!(resolved(value), kernel(value));
        // The same through a relative symlink target.
        assert_eq!(resolved("buck-out/v2/chain"), kernel("buck-out/v2/chain"));
        // `..` between declared directories agrees with the kernel and stays bound.
        let value = "buck-out/v2/gen/a/../b";
        assert_eq!(resolved(value), kernel(value));
        // `..` after a component the tree does not declare is refused, since the kernel may
        // follow a symlink there that the tree does not record.
        assert!(
            t.resolve(Path::new("buck-out/v2/undeclared/../payload"))
                .is_err()
        );

        // As command operands, none of these is accepted as the declared payload.
        for value in [
            "buck-out/v2/link/../payload",
            "buck-out/v2/undeclared/../payload",
        ] {
            let tree_without_links = tree(
                &[
                    ("buck-out/v2/payload", file_leaf(b"artifact", false)),
                    ("buck-out/v2/link", Leaf::External(outside.join("deep"))),
                ],
                &[],
            );
            // The link itself is already refused by the all-symlink rule; check the operand
            // classification separately with the classifier.
            let argv = vec![sh.clone(), value.to_owned()];
            let env = vec![(OsString::from("PATH"), OsString::from(&store))];
            let facts = LaunchFacts {
                project_root: &root,
                working_directory: vec![],
                buck_out: vec!["buck-out".into(), "v2".into()],
                argv: &argv,
                env: &env,
            };
            let classify = Classify {
                tree: &tree_without_links,
                facts: &facts,
            };
            let e = classify
                .named(value)
                .err()
                .unwrap_or_else(|| panic!("`{value}` was accepted as a bound operand"));
            assert!(e.contains(value), "{e}");
            assert!(plan(&t, &root, &[&sh, value], &[path_env]).is_err());
        }
    }

    fn tempfile_root(name: &str) -> PathBuf {
        let base = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let root = base.join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
}
