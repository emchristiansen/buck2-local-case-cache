"""Run a prost build script to generate a crate's .rs files (decl-d3).

Replaces the prelude genrule that shims.bzl `_rust_protobuf_library` used: that
genrule ran its script through the undeclared host shell `/usr/bin/env bash`
(prelude genrule.bzl `script_args`). This action instead runs the pinned bash
and the pinned coreutils mkdir (nix/pinned_tools.bzl) with nix-manifest.json as
a hidden input and TRIAL_NIX_MANIFEST naming it, as the other pinned-tool
actions do (pinned_directory.bzl, the Muninn prelude's archive and Rust actions).

It keeps the genrule's observable contract for the build scripts:
- `srcs` are placed in a fresh directory under their short paths, and the build
  script runs with that directory as its working directory (genrule `cd $SRCDIR`).
  re_grpc_proto's build.rs relies on this: it compiles `proto/...` files with
  `./proto/` as its include root. The directory is copied rather than
  symlinked, as the Muninn prelude does for every Nix-worker source directory.
- `env` (PROTOC, PROTOC_INCLUDE, BUCK_PROTO_SRCS and any build_env) is passed
  with every path made relative to that working directory, and `OUT` names the
  output directory, created before the build script runs (genrule `mkdir -p $OUT`;
  prost-build does not create it). buck2_protoc_dev hands `$OUT` to prost as its
  out_dir, because OUT_DIR is unset.
- The default output is that directory, the genrule's `out = "."`, so the
  consuming rust_library's `OUT_DIR = $(location ...)` and `named_deps` see the
  same thing.
It is not local_only: the action may run remotely.
"""

load(":pinned_tools.bzl", "PINNED_TOOLS")

# $1: working directory, $2: mkdir, rest: the build script.
_SCRIPT = 'cd "$1" && "$2" -p "$OUT" && shift 2 && exec "$@"'

def _rust_protobuf_generate_impl(ctx):
    # Path-based (not content-based) outputs, as the genrule had for out = ".".
    out = ctx.actions.declare_output("out", dir = True, has_content_based_path = False)
    srcs = {src.short_path: src for src in ctx.attrs.srcs}
    if len(srcs) != len(ctx.attrs.srcs):
        fail("rust_protobuf_generate srcs include a duplicate short path")
    srcs_dir = ctx.actions.copied_dir("srcs", srcs, has_content_based_path = False)

    env = {key: cmd_args(value, relative_to = srcs_dir) for key, value in ctx.attrs.env.items()}
    env["OUT"] = cmd_args(out.as_output(), relative_to = srcs_dir)

    # Read by the worker before the script changes directory: project-relative.
    env["TRIAL_NIX_MANIFEST"] = cmd_args(ctx.attrs.nix_manifest, replace_regex = ("^\\./", ""))

    ctx.actions.run(
        cmd_args(
            PINNED_TOOLS["bash"]["path"],
            "-e",
            "-c",
            _SCRIPT,
            "rust_protobuf_generate",
            srcs_dir,
            PINNED_TOOLS["mkdir"]["path"],
            cmd_args(ctx.attrs.build_script[RunInfo], relative_to = srcs_dir),
            hidden = [ctx.attrs.nix_manifest, out.as_output()],
        ),
        env = env,
        category = "rust_protobuf_generate",
    )
    return [DefaultInfo(default_output = out)]

rust_protobuf_generate = rule(
    impl = _rust_protobuf_generate_impl,
    attrs = {
        # The prost build script (a rust_binary), run on the execution platform.
        "build_script": attrs.exec_dep(providers = [RunInfo]),
        "env": attrs.dict(key = attrs.string(), value = attrs.arg(), sorted = False, default = {}),
        "nix_manifest": attrs.source(),
        "srcs": attrs.list(attrs.source(), default = []),
    },
)
