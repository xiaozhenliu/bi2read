#!/usr/bin/env bash
set -euo pipefail

app=${1:-}
[ -n "$app" ] && [ -d "$app/Contents" ] || {
    printf 'Usage: %s /absolute/path/BiMyScribe.app\n' "$0" >&2
    exit 2
}
case "$app" in /*) ;; *) printf 'App path must be absolute.\n' >&2; exit 2 ;; esac

manifest="$app/Contents/Resources/release-manifest.json"
[ -f "$manifest" ] || { printf 'Release manifest is missing.\n' >&2; exit 1; }
binary="$app/Contents/MacOS/bimyscribe"
uv="$app/Contents/Resources/bin/uv"
runtime_manifest="$app/Contents/Resources/runtime/bimyscribe-runtime.toml"

python3 - "$manifest" \
    "$(shasum -a 256 "$binary" | awk '{print $1}')" \
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
