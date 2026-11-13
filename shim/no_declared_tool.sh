#!/bin/sh
# Stand-in for a tool only the explicit NativeLink mode declares (see shim/BUCK); it never runs one.
echo "$0: this tool is declared only in the NativeLink mode (buck2 --config-file mode/nativelink.buckconfig)" >&2
exit 1
