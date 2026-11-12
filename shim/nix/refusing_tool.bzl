"""A declared tool that refuses to run, for a tool slot that must be filled but is unused."""

load(":pinned_tools.bzl", "PINNED_TOOLS")

def _refusing_tool_impl(ctx):
    return [
        DefaultInfo(),
        RunInfo(args = cmd_args(
            PINNED_TOOLS["bash"]["path"],
            "-c",
            "echo \"$0\" >&2; exit 1",
            ctx.attrs.message,
            hidden = [ctx.attrs.nix_manifest],
        )),
    ]

refusing_tool = rule(
    impl = _refusing_tool_impl,
    attrs = {
        "message": attrs.string(),
        "nix_manifest": attrs.source(),
    },
)
