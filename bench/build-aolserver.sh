#!/usr/bin/env bash
# Build the original C AOLserver 4.5 as a reference point for bench/run.sh.
#
# Usage: bench/build-aolserver.sh [prefix]     (default /opt/aolserver-bench)
# Then:  AOLSERVER_HOME=<prefix> bench/run.sh
#
# Builds upstream aolserver/aolserver at a pinned commit against the system
# Tcl 8.6 (the same libtcl aoloxide links).  AOLserver 4.5 predates Tcl 8.6;
# it serves fine, but logs one TclOO "Tcl exception" per new interp and
# crashes on shutdown.  Neither affects the benchmark.
set -euo pipefail

PREFIX="${1:-/opt/aolserver-bench}"
REPO=https://github.com/aolserver/aolserver.git
COMMIT=650ff5697c8b423f7cc338fbca64ac7f1d891e99   # "Migrate to Tcl_Panic() (#9)"
TCL_DIR="${TCL_DIR:-/usr/lib/tcl8.6}"             # holds tclConfig.sh
TCLSH="${TCLSH:-$(command -v tclsh8.6 || true)}"

[ -f "$TCL_DIR/tclConfig.sh" ] || { echo "no tclConfig.sh in $TCL_DIR (apt-get install tcl8.6-dev)" >&2; exit 1; }
[ -n "$TCLSH" ] || { echo "no tclsh8.6 (apt-get install tcl8.6)" >&2; exit 1; }

SRC="$(mktemp -d)"
trap 'rm -rf "$SRC"' EXIT
git clone -q "$REPO" "$SRC/aolserver"
git -C "$SRC/aolserver" checkout -q "$COMMIT"
cd "$SRC/aolserver"
./configure --prefix="$PREFIX" --with-tcl="$TCL_DIR" TCLSH="$TCLSH" > configure.log
make -j"$(nproc)" > make.log 2>&1 || { tail -20 make.log >&2; exit 1; }
make install > install.log 2>&1 || { tail -20 install.log >&2; exit 1; }
# nsd refuses to run as root; run.sh starts it with -u nobody, so the tree
# must be readable by others and the log directory (pid file) writable.
chmod -R o+rX "$PREFIX"
chmod o+rwx "$PREFIX/log"
echo "installed AOLserver ($COMMIT) to $PREFIX; now: AOLSERVER_HOME=$PREFIX bench/run.sh"
