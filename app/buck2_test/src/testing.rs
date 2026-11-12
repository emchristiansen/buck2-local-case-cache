/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Process-wide setup shared by this crate's unit tests.

use std::sync::Once;

/// Install the late-bound globals Starlark evaluation and analysis read (such as
/// `AnalysisContextReprLate`). Each implementation crate's `init_late_bindings` may run only once
/// per process, so every test module calls this one guard: a test that runs alone installs them
/// itself, and tests sharing a process never install them twice.
pub(crate) fn init_late_bindings() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        buck2_action_impl::init_late_bindings();
        buck2_analysis::init_late_bindings();
        buck2_anon_target::init_late_bindings();
        buck2_configured::init_late_bindings();
        buck2_events::init_late_bindings();
        buck2_interpreter_for_build::init_late_bindings();
        buck2_build_api::init_late_bindings();
        buck2_transition::init_late_bindings();
    });
}

/// The variable that names the declared shell: the exact Nix store path of the pinned bash
/// (`shim/nix/pinned_tools.bzl` `PINNED_TOOLS["bash"]`, a root of `shim/nix/nix-manifest.json`).
/// Under Buck, `app/buck2_test/BUCK` supplies it through `test_env` as
/// `$(exe toolchains//:archive_shell)`, whose run info is that store path with the manifest as
/// a hidden input. Under Cargo, the caller sets it to the same store path. Host state such as
/// `/bin/sh` is outside the declared closure, so no fixture reads it.
pub(crate) const DECLARED_SHELL: &str = "BUCK2_TEST_DECLARED_SHELL";

/// The declared shell, a canonical Nix store path. Panics, naming the variable, when it is not
/// set: there is no fallback to a host shell.
pub(crate) fn declared_shell() -> std::path::PathBuf {
    let shell = std::path::PathBuf::from(std::env::var_os(DECLARED_SHELL).unwrap_or_else(|| {
        panic!("{DECLARED_SHELL} is not set: it must name the declared shell's Nix store path")
    }));
    assert!(
        shell.starts_with("/nix/store/")
            && std::fs::canonicalize(&shell).ok().as_deref() == Some(&*shell),
        "{DECLARED_SHELL} is not a canonical Nix store path: {}",
        shell.display()
    );
    shell
}

/// The declared shell's store directory, which also holds `sh`: a `PATH` entry and interpreter
/// directory that the launch check admits.
pub(crate) fn declared_shell_dir() -> String {
    declared_shell()
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

/// A symlink at `dir/sh`, outside the Nix store, to the declared shell. It runs, but as a path it
/// is a host tool, which the launch check refuses.
pub(crate) fn non_store_shell(dir: &std::path::Path) -> std::path::PathBuf {
    let alias = dir.join("sh");
    std::fs::create_dir_all(dir).unwrap();
    match std::fs::remove_file(&alias) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("`{}` cannot be replaced: {e}", alias.display()),
    }
    std::os::unix::fs::symlink(declared_shell(), &alias).unwrap();
    assert!(!alias.starts_with("/nix/store/"));
    alias
}
