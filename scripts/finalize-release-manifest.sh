#!/usr/bin/env bash
set -euo pipefail

app=""
signed=0
while [ $# -gt 0 ]; do
    case "$1" in
        --signed) signed=1; shift ;;
        *) app=$1; shift ;;
    esac
done
[ -n "$app" ] && [ -d "$app/Contents" ] || {
    printf 'Usage: %s [--signed] /absolute/path/bi2read.app\n' "$0" >&2
    exit 2
}
case "$app" in /*) ;; *) printf 'App path must be absolute.\n' >&2; exit 2 ;; esac

manifest="$app/Contents/Resources/release-manifest.json"
[ -f "$manifest" ] || { printf 'Release manifest is missing.\n' >&2; exit 1; }
binary="$app/Contents/MacOS/bi2read"
uv="$app/Contents/Resources/bin/uv"
runtime_manifest="$app/Contents/Resources/runtime/bi2read-runtime.toml"
[ -f "$runtime_manifest" ] || runtime_manifest="$app/Contents/Resources/runtime/bimyscribe-runtime.toml"

# With `--signed`, the bundle is signed after this script runs, and that final
# `codesign` re-signs Contents/MacOS/bi2read (it carries the bundle's
# CodeDirectory, which references _CodeSignature/CodeResources). Any hash
# recorded here for the main executable is therefore stale by construction, and
# re-running finalize + codesign never converges: touching the manifest changes
# CodeResources, which changes the executable again. Leave the field empty and
# let codesign be the integrity evidence for signed bundles; unsigned
# (source-only) packages keep the real hash, which is their only such evidence.
if [ "$signed" -eq 1 ]; then
    binary_hash=""
else
    binary_hash=$(shasum -a 256 "$binary" | awk '{print $1}')
fi

python3 - "$manifest" \
    "$binary_hash" \
    "$(shasum -a 256 "$uv" | awk '{print $1}')" \
    "$(shasum -a 256 "$runtime_manifest" | awk '{print $1}')" <<'PY'
import json, os, sys, tempfile
path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)
data.update(binary_sha256=sys.argv[2], uv_sha256=sys.argv[3], runtime_manifest_sha256=sys.argv[4])
directory = os.path.dirname(path)
fd, temporary = tempfile.mkstemp(prefix=".release-manifest.", dir=directory)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(data, handle, ensure_ascii=False, indent=2, sort_keys=True)
        handle.write("\n")
    os.replace(temporary, path)
finally:
    if os.path.exists(temporary): os.unlink(temporary)
PY

printf 'Finalized signed nested-code hashes in %s\n' "$manifest"
