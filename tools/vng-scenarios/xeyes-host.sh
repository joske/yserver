#!/usr/bin/env bash
# Host half of xeyes.sh: the window's corners and the gap between the eyes
# must show the root, and the eyes must be there.
#   tools/vng-scenarios/xeyes-host.sh [name] [vng-shot args...]
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
name=${1:-xeyes}
shift || true
out=${VNG_OUT:-$repo/target/vng}/$name
rm -rf "$out"
"$repo/tools/vng-shot.sh" --name "$name" --timeout 300 "$@" \
    --scenario "$repo/tools/vng-scenarios/xeyes.sh" > /dev/null
python3 - "$out" <<'PY'
import glob, sys
from PIL import Image
files = glob.glob(f"{sys.argv[1]}/yserver-scanout-*.ppm")
if not files:
    sys.exit("no scanout dump")
p = Image.open(files[0]).convert("RGB").load()
root, white = (0x4F, 0x4F, 0x4F), (255, 255, 255)
# Window outer rect 100,100 1002x602 (bw 1). The eyes are sampled high and
# low, away from the pupils.
outside = [(101, 101), (1100, 101), (101, 700), (1100, 700), (601, 101), (601, 700)]
inside = [(351, 200), (851, 200), (351, 620), (851, 620)]
bad = [f"{xy} {p[xy]} not root" for xy in outside if p[xy] != root]
bad += [f"{xy} {p[xy]} not an eye" for xy in inside if p[xy] != white]
print("\n".join(bad) or "ok")
sys.exit(1 if bad else 0)
PY
