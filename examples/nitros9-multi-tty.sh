#!/usr/bin/env bash
#
# Patch a NitrOS-9 6809 Level 2 v3.3.0 Becker-port boot disk so it auto-
# starts a `tsmon` login listener on vserial channels /N1, /N2, /N3 with
# echo + CRLF translation enabled.  Once applied, you can run multiple
# `dw attach <ch>` sessions concurrently against the same emulator (or
# real CoCo) and each one gets its own NitrOS-9 login shell.
#
# Prerequisites:
#   - lwtools  (brew install lwtools)
#   - Toolshed `os9` utility — build from https://github.com/boisy/toolshed
#       cd toolshed/build/unix && make
#     then put toolshed/build/unix/os9 on your PATH (or pass --os9 PATH).
#   - A copy of nos96809l2v030300coco3_becker.dsk from
#       https://sourceforge.net/projects/nitros9/files/releases/v3.3.0/disks/
#
# Usage:
#   ./examples/nitros9-multi-tty.sh path/to/nos96809l2v030300coco3_becker.dsk
#
# The disk is patched in place — back it up first if you care about the
# original startup.
#
set -euo pipefail

OS9_BIN="${OS9_BIN:-os9}"
if ! command -v "$OS9_BIN" >/dev/null 2>&1; then
  echo "error: '$OS9_BIN' not in PATH (set OS9_BIN to the Toolshed os9 binary)" >&2
  exit 1
fi

if [ $# -lt 1 ]; then
  echo "usage: $0 <path/to/nos9-becker.dsk>" >&2
  exit 2
fi
disk="$1"
if [ ! -f "$disk" ]; then
  echo "error: $disk not found" >&2
  exit 1
fi

# OS-9 uses CR as the line terminator. Build the new startup file in a
# tempfile, then replace ,startup on the disk.
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

python3 - >"$tmp" <<'PY'
import sys
lines = [
    "echo * NitrOS-9 ready *",
    "link shell",
    "load utilpak1",
    "date -t",
    "iniz /N1",
    "iniz /N2",
    "iniz /N3",
    "xmode /N1 eko=1 alf=1 pau=0",
    "xmode /N2 eko=1 alf=1 pau=0",
    "xmode /N3 eko=1 alf=1 pau=0",
    "tsmon /N1&",
    "tsmon /N2&",
    "tsmon /N3&",
]
sys.stdout.buffer.write(("\r".join(lines) + "\r").encode("ascii"))
PY

"$OS9_BIN" del "${disk},startup" >/dev/null 2>&1 || true
"$OS9_BIN" copy "$tmp" "${disk},startup"

echo "patched $disk — startup now spins up tsmon on /N1, /N2, /N3"
