"""Engine Rust toolchain pinned to the Nix store nightly of rust-toolchain.toml.

Follows Muninn's toolchains/rust.bzl (recv-v2/ws, sha256 2aec021e...): the
compiler, rustdoc and clippy-driver are exact store paths, each carrying
nix-manifest.json as a hidden input, and the same manifest is the provider's
`trial_nix_manifest`, which the Muninn-patched prelude uses to select its Nix
worker route (copied rather than symlinked inputs, `--closed-nix-env`, and the
manifest as a hidden input of every test command).

Differences from W, each deliberate:
- the compiler is this tree's nightly (rust-toolchain.toml `nightly-2026-08-15`),
  not W's stable `rust-mixed`, and `nightly_features = True`, because the engine
  crates use `#![feature(...)]`;
- `-Clinker-features=-lld` keeps rustc from injecting its self-contained rust-lld
  (`-B<sysroot>/.../gcc-ld -fuse-ld=lld`), so the declared cc-wrapper links and
  records store RUNPATHs;
- Muninn's libtest process runner is the test process runner, as in W, because
  the Muninn prelude's portable remote test contract requires it; there is no
  Muninn lint policy and no secret route (portable tests forbid one).
"""

load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")
load(":pinned_tools.bzl", "PINNED_NIX_PATH", "PINNED_TOOLS")

def _engine_pinned_rust_toolchain_impl(ctx):
    return [
        DefaultInfo(),
        RustToolchainInfo(
            compiler = RunInfo(args = [cmd_args(PINNED_TOOLS["rustc"]["path"], hidden = [ctx.attrs.nix_manifest])]),
            clippy_driver = RunInfo(args = [cmd_args(PINNED_TOOLS["clippy_driver"]["path"], hidden = [ctx.attrs.nix_manifest])]),
            rustdoc = RunInfo(args = [cmd_args(PINNED_TOOLS["rustdoc"]["path"], hidden = [ctx.attrs.nix_manifest])]),
            linker = ctx.attrs.linker[RunInfo],
            default_edition = ctx.attrs.default_edition,
            rustc_flags = ["-Clinker-features=-lld"],
            rustc_target_triple = "x86_64-unknown-linux-gnu",
            panic_runtime = PanicRuntime("unwind"),
            nightly_features = True,
            doctests = False,
            rustc_env = {
                "PATH": PINNED_NIX_PATH,
                "CARGO": PINNED_TOOLS["cargo"]["path"],
            },
            rustdoc_env = {"PATH": PINNED_NIX_PATH},
            trial_nix_manifest = ctx.attrs.nix_manifest,
            local_case_env = {"PATH": PINNED_NIX_PATH},
            test_process_runner = RunInfo(args = cmd_args(
                ctx.attrs.test_process_runner[RunInfo],
                ["--per-test-timeout-seconds", ctx.attrs.per_test_timeout_seconds] if ctx.attrs.per_test_timeout_seconds != None else [],
                "--",
            )),
        ),
    ]

engine_pinned_rust_toolchain = rule(
    impl = _engine_pinned_rust_toolchain_impl,
    attrs = {
        "default_edition": attrs.string(default = "2024"),
        "linker": attrs.exec_dep(providers = [RunInfo]),
        "nix_manifest": attrs.source(),
        "per_test_timeout_seconds": attrs.option(attrs.string(), default = None),
        # A `dep`, as in W: tests run on the target platform.
        "test_process_runner": attrs.dep(providers = [RunInfo]),
    },
    is_toolchain_rule = True,
)
