#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s --app APP --dmg DMG --notary-result JSON --evidence-out JSON\n' "$0" >&2
    exit 2
}
app=""; dmg=""; notary_result=""; evidence_out=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --app) app=${2:?}; shift 2 ;;
        --dmg) dmg=${2:?}; shift 2 ;;
        --notary-result) notary_result=${2:?}; shift 2 ;;
        --evidence-out) evidence_out=${2:?}; shift 2 ;;
        *) usage ;;
    esac
done
for path in "$app" "$dmg" "$notary_result"; do [ -e "$path" ] || usage; done
case "$evidence_out" in /*) ;; *) usage ;; esac
[ ! -e "$evidence_out" ] || { printf 'Refusing to overwrite evidence: %s\n' "$evidence_out" >&2; exit 1; }

codesign --verify --deep --strict --verbose=2 "$app"
codesign --verify --strict --verbose=2 "$dmg"
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature "$dmg"
identity=$(codesign -dvvv "$app" 2>&1 | sed -n 's/^Authority=//p' | head -n 1)
[ -n "$identity" ] || { printf 'Cannot read Developer ID signing identity.\n' >&2; exit 1; }

tree_hash() {
    python3 - "$1" <<'PY'
import hashlib, os, stat, sys
root=os.path.realpath(sys.argv[1]); digest=hashlib.sha256()
for base,dirs,files in os.walk(root):
    dirs.sort(); files.sort()
    for name in files:
        path=os.path.join(base,name); relative=os.path.relpath(path,root)
        digest.update(relative.encode()); digest.update(b"\0"); digest.update(oct(stat.S_IMODE(os.lstat(path).st_mode)).encode()); digest.update(b"\0")
        if os.path.islink(path): digest.update(os.readlink(path).encode())
        else:
            with open(path,"rb") as handle:
                for chunk in iter(lambda:handle.read(1024*1024),b""): digest.update(chunk)
print(digest.hexdigest())
PY
}
app_hash=$(tree_hash "$app")
dmg_hash=$(shasum -a 256 "$dmg" | awk '{print $1}')
mkdir -p "$(dirname "$evidence_out")"
python3 - "$notary_result" "$evidence_out" "$identity" "$app_hash" "$dmg_hash" <<'PY'
import json, sys, time
with open(sys.argv[1],encoding="utf-8") as handle: result=json.load(handle)
request_id=result.get("id") or result.get("requestId"); status=result.get("status")
assert request_id and status in {"Accepted","accepted"}
data={"app_tree_sha256":sys.argv[4],"dmg_sha256":sys.argv[5],"notarization_request_id":request_id,
      "notarization_status":"accepted","signing_identity":sys.argv[3],"staple":"passed","gatekeeper":"passed",
      "timestamp_unix":int(time.time())}
with open(sys.argv[2],"x",encoding="utf-8") as handle: json.dump(data,handle,indent=2,sort_keys=True); handle.write("\n")
print(json.dumps(data,sort_keys=True))
PY
