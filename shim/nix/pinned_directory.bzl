"""Copy exact subpaths of a pinned store directory into content-hashed Buck outputs."""

load("@prelude//http_archive:exec_deps.bzl", "PinnedExecutableInfo")

def _pinned_directory_copy_impl(ctx):
    # The pinned interpreter runs the copy; like the archive action, it is handed the
    # declared manifest of the closure whose store path it reads.
    manifest = cmd_args(ctx.attrs.nix_manifest, replace_regex = ("^\\./", ""))
    arguments = [ctx.attrs._python[RunInfo], ctx.attrs.script, "--root", ctx.attrs.path]
    sub_targets = {}
    for subpath, kind in ctx.attrs.subpaths.items():
        output = ctx.actions.declare_output(subpath, dir = kind == "directory")
        arguments += ["--copy", kind, subpath, output.as_output()]
        sub_targets[subpath] = [DefaultInfo(default_output = output)]
    ctx.actions.run(
        cmd_args(arguments, hidden = [ctx.attrs.nix_manifest]),
        env = {"TRIAL_NIX_MANIFEST": manifest},
        category = "pinned_directory_copy",
    )
    return [DefaultInfo(sub_targets = sub_targets)]

pinned_directory_copy = rule(
    impl = _pinned_directory_copy_impl,
    attrs = {
        # A PINNED_TOOLS directory artifact's exact store path.
        "path": attrs.string(),
        "nix_manifest": attrs.source(),
        "script": attrs.source(),
        # Each subpath copied, by its exact type; each is a sub-target of that name.
        "subpaths": attrs.dict(attrs.string(), attrs.enum(["directory", "file"])),
        "_python": attrs.exec_dep(providers = [RunInfo, PinnedExecutableInfo], default = "toolchains//:python_interpreter"),
    },
)
