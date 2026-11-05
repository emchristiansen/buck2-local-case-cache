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
//! This classifies operands; it does not discover dependencies. What a bound artifact itself
//! loads or reads while it runs is covered by the provider's explicit request, which declares
//! that its cases are deterministic within their declared inputs.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::directory_iterator::DirectoryIterator;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionImmutableDirectory;

/// Symlink hops followed while resolving one path, as the kernel's limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// The kernel reads at most this many bytes of a `#!` line.
const SHEBANG_LIMIT: usize = 256;

const LOADER_OVERRIDES: &[&str] = &["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT"];

/// One leaf of the action's input tree.
#[derive(Debug, Clone)]
enum Leaf {
    File { executable: bool },
    /// A symlink whose target is relative to its own directory.
    Symlink(String),
    /// A symlink to an absolute host path.
    External(PathBuf),
}

/// The action's input tree, by project-relative path components.
#[derive(Debug)]
pub(crate) struct InputTree {
    leaves: HashMap<Vec<String>, Leaf>,
    dirs: HashSet<Vec<String>>,
}

/// Where a path ends after following input-tree symlinks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    Tree { path: Vec<String>, executable: bool },
    TreeDir,
    Host(PathBuf),
    /// A project path the input tree does not contain.
    Undeclared(Vec<String>),
}

impl InputTree {
    pub(crate) fn new(directory: &ActionImmutableDirectory) -> Self {
        let mut leaves = HashMap::new();
        let mut dirs = HashSet::new();
        for (path, entry) in directory.unordered_walk().with_paths() {
            let components = path.iter().map(|c| c.as_str().to_owned()).collect::<Vec<_>>();
            let leaf = match entry {
                DirectoryEntry::Dir(_) => {
                    dirs.insert(components);
                    continue;
                }
                DirectoryEntry::Leaf(ActionDirectoryMember::File(meta)) => Leaf::File {
                    executable: meta.is_executable,
                },
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

    fn resolve(&self, mut path: Vec<String>) -> Result<Resolved, String> {
        let mut hops = 0;
        'restart: loop {
            for end in 1..=path.len() {
                match self.leaves.get(&path[..end]) {
                    None => {}
                    Some(Leaf::File { executable }) => {
                        return Ok(if end == path.len() {
                            Resolved::Tree {
                                path,
                                executable: *executable,
                            }
                        } else {
                            Resolved::Undeclared(path)
                        });
                    }
                    Some(Leaf::External(target)) => {
                        let mut host = target.clone();
                        host.extend(&path[end..]);
                        return Ok(Resolved::Host(host));
                    }
                    Some(Leaf::Symlink(target)) => {
                        hops += 1;
                        if hops > MAX_SYMLINK_HOPS {
                            return Err(format!(
                                "`{}` passes more than {MAX_SYMLINK_HOPS} input-tree symlinks",
                                path.join("/")
                            ));
                        }
                        let mut next = path[..end - 1].to_vec();
                        push_relative(&mut next, Path::new(target)).ok_or_else(|| {
                            format!(
                                "input-tree symlink `{}` -> `{target}` leaves the project",
                                path[..end].join("/")
                            )
                        })?;
                        next.extend_from_slice(&path[end..]);
                        path = next;
                        continue 'restart;
                    }
                }
            }
            return Ok(if self.dirs.contains(&path) {
                Resolved::TreeDir
            } else {
                Resolved::Undeclared(path)
            });
        }
    }
}

/// Append a relative path to `base`, applying `.` and `..`; `None` if it is not relative or
/// leaves the root.
fn push_relative(base: &mut Vec<String>, path: &Path) -> Option<()> {
    for component in path.components() {
        match component {
            Component::Normal(c) => base.push(c.to_str()?.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop()?;
            }
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
    Artifact { path: Vec<String>, executable: bool },
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

/// The input-tree executables the command launches, whose `#!` lines must be read once the
/// inputs are on disk.
#[derive(Debug, Clone)]
pub(crate) struct LaunchPlan {
    project_root: PathBuf,
    executables: Vec<Vec<String>>,
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

        let classify = Classify { tree, facts: &facts };
        let mut executables = Vec::new();
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
        Ok(Self {
            project_root: facts.project_root.to_owned(),
            executables,
        })
    }

    /// The input-tree executables to materialize before [`LaunchPlan::verify`].
    pub(crate) fn executables(&self) -> Vec<ProjectRelativePathBuf> {
        self.executables
            .iter()
            .map(|p| ProjectRelativePathBuf::unchecked_new(p.join("/")))
            .collect()
    }

    /// Check that each launched input-tree script's `#!` interpreter is a Nix store path. The
    /// executables must be materialized.
    pub(crate) fn verify(&self) -> Result<(), String> {
        for path in &self.executables {
            let mut disk = self.project_root.clone();
            disk.extend(path);
            let mut head = Vec::with_capacity(SHEBANG_LIMIT);
            {
                use std::io::Read;
                std::fs::File::open(&disk)
                    .and_then(|f| f.take(SHEBANG_LIMIT as u64).read_to_end(&mut head))
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
        let project = if path.is_absolute() {
            match path.strip_prefix(self.facts.project_root) {
                Ok(inside) => {
                    let mut components = Vec::new();
                    push_relative(&mut components, inside)
                        .ok_or_else(|| format!("`{value}` is not a plain project path"))?;
                    components
                }
                Err(_) => return self.host(path, value),
            }
        } else {
            let mut components = self.facts.working_directory.clone();
            push_relative(&mut components, path)
                .ok_or_else(|| format!("`{value}` leaves the project"))?;
            components
        };
        match self.tree.resolve(project)? {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(leaves: &[(&str, Leaf)], dirs: &[&str]) -> InputTree {
        let split = |p: &str| p.split('/').map(str::to_owned).collect::<Vec<_>>();
        InputTree {
            leaves: leaves
                .iter()
                .map(|(p, l)| (split(p), l.clone()))
                .collect(),
            dirs: dirs.iter().map(|d| split(d)).collect(),
        }
    }

    fn store_dir() -> String {
        // Any canonical directory in the store of the machine running the test.
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        assert!(sh.starts_with("/nix/store"), "tests need a Nix store host");
        sh.parent().unwrap().to_str().unwrap().to_owned()
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
            &[(script, Leaf::File { executable: true })],
            &["buck-out", "buck-out/v2", "buck-out/v2/gen", "buck-out/v2/gen/t"],
        );

        let e = plan(&t, &root, &["/bin/sh", script], &[path_env]).unwrap_err();
        assert!(e.contains("/bin/sh"), "{e}");
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
        let e = plan(&t, &root, &[&sh, script], &[path_env, ("F", "x=/etc/hostname")])
            .unwrap_err();
        assert!(e.contains("/etc/hostname"), "{e}");
        // Values naming nothing that exists are not file operands.
        assert!(plan(&t, &root, &[&sh, script, "case_a", "/no/such/file"], &[path_env]).is_ok());

        std::fs::write(root.join("undeclared.txt"), "live").unwrap();
        let e = plan(&t, &root, &[&sh, script, "undeclared.txt"], &[path_env]).unwrap_err();
        assert!(e.contains("undeclared project file"), "{e}");

        // A declared checkout file is an input, but no build artifact.
        let with_source = tree(
            &[
                (script, Leaf::File { executable: true }),
                ("src/lib.rs", Leaf::File { executable: false }),
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
        let t = tree(
            &[
                ("buck-out/v2/gen/t/script", Leaf::File { executable: true }),
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

        std::fs::write(dir.join("script"), "#!/bin/sh\necho\n").unwrap();
        let p = plan(&t, &root, &["buck-out/v2/gen/u/link"], &[path_env]).unwrap();
        assert_eq!(
            p.executables(),
            vec![ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/gen/t/script".to_owned()
            )]
        );
        let e = p.verify().unwrap_err();
        assert!(e.contains("/bin/sh"), "{e}");

        std::fs::write(dir.join("script"), format!("#!{store}/sh -e\necho\n")).unwrap();
        p.verify().unwrap();
        std::fs::write(dir.join("script"), b"\x7fELF binary").unwrap();
        p.verify().unwrap();

        // An external symlink into the store is a bound tool.
        let p = plan(&t, &root, &["buck-out/v2/gen/t/tool"], &[path_env]).unwrap();
        assert!(p.executables().is_empty());
    }

    fn tempfile_root(name: &str) -> PathBuf {
        let base = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let root = base.join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
}
