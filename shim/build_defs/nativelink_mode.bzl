# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Whether the explicit workstation NativeLink mode (mode/nativelink.buckconfig) is selected.

Without that file the tree builds with the bundled prelude and the shim's system toolchains, the
upstream-compatible default. With it, every NativeLink setting must be in effect: a mode that is
selected but only partly applied fails here instead of building some other way.
"""

# Settings mode/nativelink.buckconfig makes. It is command-line configuration, so every cell sees
# it, and each is read from the cell evaluating this code.
_MODE_REQUIRED = [
    ("build", "execution_platforms", "shim//platforms:remote_linux"),
]

# The remote execution client's startup settings. The daemon reads them only from the root cell's
# project configuration (mode/nativelink-startup.buckconfig, copied to the root .buckconfig.local),
# and another cell's configuration holds only its own project files and command-line configuration,
# so they are read with `read_root_config` whichever cell evaluates this code.
# mode/nativelink_preflight.py refuses them as command-line configuration, where only this check
# would see them.
_STARTUP_REQUIRED = [
    ("buck2_re_client", "engine_address", "grpc://127.0.0.1:50051"),
    ("buck2_re_client", "action_cache_address", "grpc://127.0.0.1:50051"),
    ("buck2_re_client", "cas_address", "grpc://127.0.0.1:50051"),
]

def nativelink_mode():
    """True in the explicit NativeLink mode, False in the default; fails on a partial mode."""
    mode = read_config("shim", "toolchain_mode", "")
    if mode == "":
        return False
    if mode != "nativelink":
        fail("shim.toolchain_mode is {}; the only mode is nativelink (mode/nativelink.buckconfig)".format(repr(mode)))
    for required, read in [(_MODE_REQUIRED, read_config), (_STARTUP_REQUIRED, read_root_config)]:
        for section, key, value in required:
            found = read(section, key, "")
            if found != value:
                fail("the NativeLink mode needs {}.{} = {}, but it is {}".format(section, key, value, repr(found)))
    return True

def require_nativelink_mode(what):
    """Fail unless the explicit NativeLink mode is selected and complete; `what` names the caller."""
    if not nativelink_mode():
        fail("{} belongs to the NativeLink mode, selected only by mode/nativelink.buckconfig".format(what))
