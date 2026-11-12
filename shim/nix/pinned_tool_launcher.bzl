"""A pinned store tool as a Buck artifact, for a `$(location ...)` that must name a file.

`pinned_executable` has no default output, so `$(location ...)`/`$(location_exec ...)`
of it expands to nothing. A test environment variable that upstream fills with a tool's
location (gazebo/hmap's HMAP_TEST_CLANG) needs a real path, so this rule writes a
two-line launcher, run by the pinned shell, that execs the exact store path with every
argument unchanged. The Nix manifest of the pinned closure rides along as another output,
so whatever references the launcher also carries the closure it needs.
"""

load(":pinned_tools.bzl", "PINNED_TOOLS")

def _pinned_tool_launcher_impl(ctx):
    launcher = ctx.actions.write(
        ctx.attrs.name,
        [
            "#!" + PINNED_TOOLS["bash"]["path"],
            "exec " + ctx.attrs.path + ' "$@"',
        ],
        is_executable = True,
    )
    return [
        DefaultInfo(default_output = launcher, other_outputs = [ctx.attrs.nix_manifest]),
        RunInfo(args = cmd_args(launcher, hidden = [ctx.attrs.nix_manifest])),
    ]

pinned_tool_launcher = rule(
    impl = _pinned_tool_launcher_impl,
    attrs = {
        "nix_manifest": attrs.source(),
        # A PINNED_TOOLS executable's exact store path.
        "path": attrs.string(),
    },
)
