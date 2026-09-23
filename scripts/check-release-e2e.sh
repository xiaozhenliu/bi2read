#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s --app APP --release-check-root DIR --evidence FILE [--protected-path PATH ...]\n' "$0" >&2
    exit 2
}

app=""; check_root=""; evidence=""; protected_paths=()
while [ "$#" -gt 0 ]; do
    case "$1" in
        --app) app=${2:?}; shift 2 ;;
        --release-check-root) check_root=${2:?}; shift 2 ;;
        --evidence) evidence=${2:?}; shift 2 ;;
        --protected-path) protected_paths+=("${2:?}"); shift 2 ;;
        *) usage ;;
    esac
done
[ -x "$app/Contents/MacOS/bi2read" ] || usage
case "$check_root" in /*) ;; *) usage ;; esac
case "$evidence" in /*) ;; *) usage ;; esac
[ ! -e "$check_root" ] || { printf 'release-check root must be fresh: %s\n' "$check_root" >&2; exit 1; }
[ ! -e "$evidence" ] || { printf 'Refusing to overwrite evidence: %s\n' "$evidence" >&2; exit 1; }

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
config="$repo_root/packaging/release.toml"
fixture_values=$(python3 - "$config" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as handle: fixture=tomllib.load(handle)["fixture"]
for key in ("url", "bvid", "page", "minimum_utterances", "maximum_duration_ms"): print(fixture[key])
PY
)
fixture_url=$(printf '%s\n' "$fixture_values" | sed -n '1p')
fixture_bvid=$(printf '%s\n' "$fixture_values" | sed -n '2p')
fixture_page=$(printf '%s\n' "$fixture_values" | sed -n '3p')
minimum_utterances=$(printf '%s\n' "$fixture_values" | sed -n '4p')
maximum_duration_ms=$(printf '%s\n' "$fixture_values" | sed -n '5p')
binary="$app/Contents/MacOS/bi2read"
check_token=$(uuidgen | tr -d '-' | tr '[:upper:]' '[:lower:]')

hash_path() {
    python3 - "$1" <<'PY'
import hashlib, os, sys
root=sys.argv[1]
if not os.path.exists(root): print("absent"); raise SystemExit
digest=hashlib.sha256()
paths=[root] if os.path.isfile(root) else [os.path.join(base,name) for base,_,names in os.walk(root) for name in names]
for path in sorted(paths):
    digest.update(os.path.relpath(path,root).encode()); digest.update(b"\0")
    with open(path,"rb") as handle:
        for chunk in iter(lambda:handle.read(1024*1024),b""): digest.update(chunk)
print(digest.hexdigest())
PY
}
before_file=$(mktemp /private/tmp/bi2read-protected-before.XXXXXX)
after_file=$(mktemp /private/tmp/bi2read-protected-after.XXXXXX)
trap 'rm -f "$before_file" "$after_file"' EXIT
for path in "${protected_paths[@]}"; do printf '%s %s\n' "$(hash_path "$path")" "$path"; done > "$before_file"

status_output=$($binary --release-check-root "$check_root" --release-check-token "$check_token" runtime status 2>&1 || true)
printf '%s\n' "$status_output" | rg -q '^source: bundled$' || { printf '%s\n' "$status_output" >&2; exit 1; }
printf '%s\n' "$status_output" | rg -q '^backend: 原生 uv$' || { printf '%s\n' "$status_output" >&2; exit 1; }
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    printf 'Docker must be unavailable during the final fixture E2E.\n' >&2
    exit 1
fi
docker_available=false
$binary --release-check-root "$check_root" --release-check-token "$check_token" runtime install
result=$($binary --release-check-root "$check_root" --release-check-token "$check_token" transcribe "$fixture_url" --no-llm \
    --retention keep-all --work-dir "$check_root/work" --output-dir "$check_root/output" --json)

bundled_runtime_manifest="$app/Contents/Resources/runtime/bi2read-runtime.toml"
[ -f "$bundled_runtime_manifest" ] || bundled_runtime_manifest="$app/Contents/Resources/runtime/bimyscribe-runtime.toml"
python3 - "$result" "$fixture_bvid" "$fixture_page" "$minimum_utterances" "$maximum_duration_ms" \
    "$check_root" "$status_output" "$docker_available" "$evidence" \
    "$bundled_runtime_manifest" <<'PY'
import hashlib, json, os, re, sys, time, tomllib
result=json.loads(sys.argv[1]); bvid=sys.argv[2]; page=int(sys.argv[3]); minimum=int(sys.argv[4]); maximum=int(sys.argv[5])
root=os.path.realpath(sys.argv[6]); status=sys.argv[7]; docker_available=sys.argv[8]=="true"; evidence=sys.argv[9]; runtime_path=sys.argv[10]
assert result["status"]=="completed" and result["bvid"]==bvid and result["page"]==page
work=os.path.realpath(result["work_dir"]); document=os.path.realpath(result["document"])
assert os.path.commonpath([root, work])==root and os.path.commonpath([root, document])==root
required=("metadata.json","transcript.raw.json","transcript.raw.md","transcript.readable.md","full.md")
for name in required:
    path=document if name=="full.md" else os.path.join(work,name)
    assert os.path.isfile(path) and os.path.getsize(path)>0, path
with open(os.path.join(work,"metadata.json"), encoding="utf-8") as handle: metadata=json.load(handle)
assert metadata["bvid"]==bvid and metadata["page"]==page and metadata["job_id"]==result["job_id"]
assert isinstance(metadata["cid"],int) and metadata["cid"]>0 and isinstance(metadata["title"],str) and metadata["title"].strip()
assert 0 < metadata["duration_ms"] <= maximum
with open(os.path.join(work,"transcript.raw.json"), encoding="utf-8") as handle: utterances=json.load(handle)
assert len(utterances)>=minimum
assert all(set(x)=={"id","text","start_ms","end_ms","speaker_id"} and isinstance(x["id"],str)
           and isinstance(x["text"],str) and x["text"].strip() and isinstance(x["speaker_id"],int)
           and 0<=x["start_ms"]<=x["end_ms"]<=maximum for x in utterances)
with open(os.path.join(work,"transcript.raw.md"), encoding="utf-8") as handle: raw=handle.read()
with open(os.path.join(work,"transcript.readable.md"), encoding="utf-8") as handle: readable=handle.read()
with open(document, encoding="utf-8") as handle: full=handle.read()
assert all(x["text"] in raw and x["text"] in readable for x in utterances)
assert all(re.search(r'https://www\.bilibili\.com/video/'+re.escape(bvid)+r'.*t='+str(x["start_ms"]//1000)+r'\)',raw) for x in utterances)
assert readable.strip() in full and bvid in full and result["job_id"] in raw and result["job_id"] in full
with open(runtime_path,"rb") as handle: runtime=tomllib.load(handle)
assert runtime["backend"]=="native-uv" and runtime["output_schema_version"]==1
schema_path=os.path.join(os.path.dirname(runtime_path),runtime["output_schema_file"])
with open(schema_path,encoding="utf-8") as handle: schema=json.load(handle)
assert isinstance(schema,dict) and schema.get("type")=="array"
schema_sha=hashlib.sha256(open(schema_path,"rb").read()).hexdigest()
data={"docker_command_available":docker_available,"fixture":{"bvid":bvid,"page":page},
      "output":{"document":document,"duration_ms":metadata["duration_ms"],"job_id":result["job_id"],"utterances":len(utterances),"work_dir":work},
      "runtime":{"backend":"native-uv","source":"bundled","status":status},
      "schema":{"file":runtime["output_schema_file"],"sha256":schema_sha,"version":runtime["output_schema_version"]},
      "timestamp_unix":int(time.time())}
os.makedirs(os.path.dirname(evidence), exist_ok=True)
with open(evidence,"x",encoding="utf-8") as handle: json.dump(data,handle,ensure_ascii=False,indent=2,sort_keys=True); handle.write("\n")
PY

for path in "${protected_paths[@]}"; do printf '%s %s\n' "$(hash_path "$path")" "$path"; done > "$after_file"
cmp -s "$before_file" "$after_file" || { printf 'Protected development/production state changed.\n' >&2; diff -u "$before_file" "$after_file" >&2 || true; exit 1; }
printf 'Final bundled native-uv fixture E2E passed; evidence: %s\n' "$evidence"
