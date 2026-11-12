"""Declare an executable at an exact prequalified store path."""

load("@prelude//http_archive:exec_deps.bzl", "PinnedExecutableInfo")
load(":pinned_tools.bzl", "PINNED_NIX_PATH")

def _pinned_executable_impl(ctx):
    executable = cmd_args(ctx.attrs.path, hidden = [ctx.attrs.nix_manifest])
    return [
        DefaultInfo(),
        RunInfo(args = [executable]),
        PinnedExecutableInfo(
            nix_manifest = ctx.attrs.nix_manifest,
            path_env = ctx.attrs.path_env or PINNED_NIX_PATH,
        ),
    ]

pinned_executable = rule(
    impl = _pinned_executable_impl,
    attrs = {
        "nix_manifest": attrs.source(),
        "path": attrs.string(),
        "path_env": attrs.option(attrs.string(), default = None),
    },
)
