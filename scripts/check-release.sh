#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s STAGE [options]\nStages: source runtime app dmg approve published\n' "$0" >&2
    exit 2
}

stage=${1:-}
[ -n "$stage" ] || usage
shift
repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
config="$repo_root/packaging/release.toml"

json_result() {
    python3 - "$@" <<'PY'
import json, sys
stage, status, expected, actual, recovery = sys.argv[1:]
print(json.dumps({"actual": actual, "expected": expected, "recovery": recovery,
                  "stage": stage, "status": status}, ensure_ascii=False, sort_keys=True))
PY
}

fail() {
    printf '[%s] FAIL: expected %s; actual %s; recovery: %s\n' "$stage" "$1" "$2" "$3" >&2
    json_result "$stage" failure "$1" "$2" "$3"
    exit 1
}

pass() {
    printf '[%s] PASS: %s\n' "$stage" "$1" >&2
    json_result "$stage" success "$1" "$1" none
}

value() {
    python3 - "$config" "$1" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as handle: data = tomllib.load(handle)
item = data.get(sys.argv[2])
if item is None: raise SystemExit(2)
print(item)
PY
}

issue=""; build_root=""; runtime_repo=""; app=""; dmg=""; sha_file=""
repo_slug=""; tag=""; public_revision=""; private_revision=""; download_dir=""; candidate="private"
release_check_root=""; e2e_evidence=""; protected_paths=(); frozen_sha256=""; approval_file=""
runtime_evidence=""
publicignore=""
evidence_out=""; private_source_evidence=""; public_source_evidence=""; runtime_gate_evidence=""; app_evidence=""; dmg_evidence=""
postdownload_dmg_evidence=""
signing_evidence=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --issue) issue=${2:?}; shift 2 ;;
        --build-root) build_root=${2:?}; shift 2 ;;
        --runtime-repo) runtime_repo=${2:?}; shift 2 ;;
        --app) app=${2:?}; shift 2 ;;
        --dmg) dmg=${2:?}; shift 2 ;;
        --sha256-file) sha_file=${2:?}; shift 2 ;;
        --repo) repo_slug=${2:?}; shift 2 ;;
        --tag) tag=${2:?}; shift 2 ;;
        --public-revision) public_revision=${2:?}; shift 2 ;;
        --private-revision) private_revision=${2:?}; shift 2 ;;
        --candidate) candidate=${2:?}; shift 2 ;;
        --download-dir) download_dir=${2:?}; shift 2 ;;
        --release-check-root) release_check_root=${2:?}; shift 2 ;;
        --e2e-evidence) e2e_evidence=${2:?}; shift 2 ;;
        --protected-path) protected_paths+=("${2:?}"); shift 2 ;;
        --frozen-sha256) frozen_sha256=${2:?}; shift 2 ;;
        --approval-file) approval_file=${2:?}; shift 2 ;;
        --runtime-evidence) runtime_evidence=${2:?}; shift 2 ;;
        --publicignore) publicignore=${2:?}; shift 2 ;;
        --evidence-out) evidence_out=${2:?}; shift 2 ;;
        --private-source-evidence) private_source_evidence=${2:?}; shift 2 ;;
        --public-source-evidence) public_source_evidence=${2:?}; shift 2 ;;
        --runtime-gate-evidence) runtime_gate_evidence=${2:?}; shift 2 ;;
        --app-evidence) app_evidence=${2:?}; shift 2 ;;
        --dmg-evidence) dmg_evidence=${2:?}; shift 2 ;;
        --postdownload-dmg-evidence) postdownload_dmg_evidence=${2:?}; shift 2 ;;
        --signing-evidence) signing_evidence=${2:?}; shift 2 ;;
        *) usage ;;
    esac
done

[ -f "$config" ] || fail "release config" missing "create packaging/release.toml"
app_version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "$repo_root/Cargo.toml" | head -n 1)
build_number=$(value build_number) || fail "build_number" missing "update packaging/release.toml"
previous_build_number=$(value previous_build_number) || fail "previous_build_number" missing "update packaging/release.toml"
runtime_tag=$(value runtime_tag) || fail "runtime_tag" missing "update packaging/release.toml"
runtime_revision=$(value runtime_revision) || fail "runtime_revision" missing "update packaging/release.toml"
runtime_contract=$(value runtime_contract) || fail "runtime_contract" missing "update packaging/release.toml"
uv_version=$(value uv_version) || fail "uv_version" missing "update packaging/release.toml"
target_platform=$(value target_platform) || fail "target_platform" missing "update packaging/release.toml"
target_arch=$(value target_arch) || fail "target_arch" missing "update packaging/release.toml"
notes_path=$(value release_notes_path) || fail "release_notes_path" missing "update packaging/release.toml"
config_sha256=$(shasum -a 256 "$config" | awk '{print $1}')

write_evidence() {
    [ -n "$evidence_out" ] || fail "explicit evidence output" missing "pass --evidence-out"
    case "$evidence_out" in /*) ;; *) fail "absolute evidence output" "$evidence_out" "use an approved absolute evidence path" ;; esac
    [ ! -e "$evidence_out" ] || fail "nonexistent evidence output" exists "choose a new evidence path"
    mkdir -p "$(dirname "$evidence_out")"
    python3 - "$evidence_out" "$stage" "$app_version" "$build_number" "$runtime_tag" "$runtime_revision" "$uv_version" "$target_platform" "$target_arch" "$config_sha256" "$@" <<'PY'
import json, sys, time
path,stage,app_version,build_number,runtime_tag,runtime_revision,uv_version,platform,arch,config_sha,*pairs=sys.argv[1:]
data={"stage":stage,"status":"passed","timestamp_unix":int(time.time()),"inputs":{"app_version":app_version,
"build_number":int(build_number),"runtime_tag":runtime_tag,"runtime_revision":runtime_revision,"uv_version":uv_version,
"target_platform":platform,"target_arch":arch,"release_config_sha256":config_sha}}
for pair in pairs:
    key,value=pair.split("=",1); data[key]=value
with open(path,"x",encoding="utf-8") as handle: json.dump(data,handle,ensure_ascii=False,indent=2,sort_keys=True); handle.write("\n")
PY
}

case "$stage" in
source)
    case "$candidate" in private|public) ;; *) fail "candidate private or public" "$candidate" "pass --candidate private|public" ;; esac
    [ -n "$issue" ] && [ -f "$issue" ] || fail "existing release issue" "${issue:-missing}" "create and pass the release issue"
    [ -n "$build_root" ] || fail "explicit absolute build root" missing "pass --build-root"
    case "$build_root" in /*) ;; *) fail "absolute build root" "$build_root" "choose an approved external absolute path" ;; esac
    [ -z "$(git -C "$repo_root" status --porcelain --untracked-files=all)" ] || fail "clean committed candidate" dirty "commit the private candidate and rerun source"
    cargo_lock_version=$(python3 - "$repo_root/Cargo.lock" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as handle: data = tomllib.load(handle)
print(next(p["version"] for p in data["package"] if p["name"] == "bimyscribe"))
PY
)
    [ "$cargo_lock_version" = "$app_version" ] || fail "$app_version in Cargo.lock" "$cargo_lock_version" "synchronize Cargo.toml and Cargo.lock"
    [ "$notes_path" = "docs/releases/v$app_version.md" ] || fail "docs/releases/v$app_version.md" "$notes_path" "fix release_notes_path"
    [ -f "$repo_root/$notes_path" ] || fail "versioned Release Notes" missing "create $notes_path"
    [ -f "$repo_root/CHANGELOG.md" ] || fail "CHANGELOG.md" missing "add the bilingual version summary"
    for heading in '## 主要变化' '## 安装' '## 兼容性与升级' '## 已知限制' '## English' '### Highlights' '### Installation' '### Compatibility and upgrades' '### Known limitations'; do
        rg -q -F "$heading" "$repo_root/$notes_path" || fail "Release Notes heading $heading" missing "complete the bilingual versioned notes"
    done
    cmp -s "$repo_root/RELEASE_NOTES.md" "$repo_root/$notes_path" || fail "RELEASE_NOTES.md byte-identical to $notes_path" different "regenerate the compatibility file from versioned notes"
    rg -q "## $app_version" "$repo_root/CHANGELOG.md" || fail "Changelog entry $app_version" missing "update CHANGELOG.md"
    for key in APP_VERSION BUILD_NUMBER PREVIOUS_RELEASE PREVIOUS_PRIVATE_REVISION PREVIOUS_BUILD_NUMBER CHANGE_CLASSIFICATION SEMVER_IMPACT RUNTIME_TAG RUNTIME_REVISION UV_VERSION TARGET_PLATFORM TARGET_ARCH RELEASE_NOTES_PATH BUILD_ROOT AUTHORIZED_ACTIONS; do
        rg -q "^$key:" "$issue" || fail "release issue field $key" missing "record all frozen inputs in the release issue"
    done
    issue_value() { sed -n "s/^$1:[[:space:]]*//p" "$issue" | head -n 1; }
    head_revision=$(git -C "$repo_root" rev-parse HEAD)
    if [ "$candidate" = private ]; then
        [ -n "$private_revision" ] || fail "source private revision" missing "pass --private-revision from the frozen candidate HEAD"
        printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || fail "full private revision" "$private_revision" "pass the full frozen candidate SHA"
        [ "$private_revision" = "$head_revision" ] || fail "private HEAD $head_revision" "$private_revision" "check out the frozen private candidate"
        expected_private_revision=$private_revision
    else
        [ -n "$public_revision" ] || fail "public revision" missing "pass --public-revision"
        [ "$public_revision" = "$head_revision" ] || fail "public HEAD $head_revision" "$public_revision" "check out the recorded public candidate"
        [ -n "$private_revision" ] || fail "source private revision" missing "pass --private-revision from the public candidate commit metadata"
        printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || fail "full private revision" "$private_revision" "pass the full source private SHA"
        expected_private_revision=$private_revision
    fi
    for pair in \
        "APP_VERSION:$app_version" "BUILD_NUMBER:$build_number" "PREVIOUS_BUILD_NUMBER:$previous_build_number" \
        "RUNTIME_TAG:$runtime_tag" "RUNTIME_REVISION:$runtime_revision" \
        "UV_VERSION:$uv_version" "TARGET_PLATFORM:$target_platform" \
        "TARGET_ARCH:$target_arch" "RELEASE_NOTES_PATH:$notes_path" "BUILD_ROOT:$build_root"; do
        key=${pair%%:*}; expected_value=${pair#*:}; actual_value=$(issue_value "$key")
        [ "$actual_value" = "$expected_value" ] || fail "$key=$expected_value" "$key=${actual_value:-missing}" "update the release issue or repository inputs"
    done
    [ "$build_number" -gt "$previous_build_number" ] || fail "build_number > $previous_build_number" "$build_number" "increase the persistent global Build Number"
    classification=$(issue_value CHANGE_CLASSIFICATION)
    case "$classification" in app-only|runtime-change|same-source-repackage) ;; *) fail "unambiguous change classification" "$classification" "choose app-only, runtime-change or same-source-repackage" ;; esac
    [ -n "$(issue_value PREVIOUS_RELEASE)" ] || fail "previous release identity" empty "record the previous public release"
    [ -n "$(issue_value AUTHORIZED_ACTIONS)" ] || fail "explicit permission boundary" empty "record authorized external actions"
    if [ "$candidate" = private ]; then
        previous_private=$(issue_value PREVIOUS_PRIVATE_REVISION)
        printf '%s' "$previous_private" | rg -q '^[0-9a-f]{40}$' || fail "full previous private revision" "$previous_private" "record PREVIOUS_PRIVATE_REVISION"
        git -C "$repo_root" merge-base --is-ancestor "$previous_private" "$head_revision" || fail "previous private revision ancestral to candidate" "$previous_private" "resolve the release baseline"
        previous_version=$(git -C "$repo_root" show "$previous_private:Cargo.toml" | sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' | head -n 1)
        previous_release_config=$(git -C "$repo_root" show "$previous_private:packaging/release.toml" 2>/dev/null || true)
        previous_runtime_tag=$(printf '%s\n' "$previous_release_config" | sed -n 's/^runtime_tag = "\([^"]*\)"/\1/p')
        previous_runtime_revision=$(printf '%s\n' "$previous_release_config" | sed -n 's/^runtime_revision = "\([^"]*\)"/\1/p')
        if [ -z "$previous_runtime_tag" ] || [ -z "$previous_runtime_revision" ]; then
            baseline="$repo_root/packaging/release-baseline.toml"
            baseline_values=$(python3 - "$baseline" "$previous_private" <<'PY'
import sys, tomllib
with open(sys.argv[1],"rb") as handle: data=tomllib.load(handle)
assert data["private_revision"]==sys.argv[2]
for key in ("release","app_version","build_number","runtime_tag","runtime_revision"): print(data[key])
PY
) || fail "matching one-time release baseline" missing "record the pre-release.toml baseline"
            baseline_release=$(printf '%s\n' "$baseline_values" | sed -n '1p')
            baseline_version=$(printf '%s\n' "$baseline_values" | sed -n '2p')
            baseline_build=$(printf '%s\n' "$baseline_values" | sed -n '3p')
            previous_runtime_tag=$(printf '%s\n' "$baseline_values" | sed -n '4p')
            previous_runtime_revision=$(printf '%s\n' "$baseline_values" | sed -n '5p')
            [ "$baseline_release/$baseline_version/$baseline_build" = "$(issue_value PREVIOUS_RELEASE)/$previous_version/$previous_build_number" ] || fail "baseline matching previous release/version/build" mismatch "correct release-baseline.toml or the issue"
        fi
        [ -n "$previous_version" ] && [ -n "$previous_runtime_tag" ] && [ -n "$previous_runtime_revision" ] || fail "machine-readable previous release inputs" missing "record the historical release inputs"
        [ "$(issue_value PREVIOUS_RELEASE)" = "v$previous_version" ] || fail "PREVIOUS_RELEASE=v$previous_version" "$(issue_value PREVIOUS_RELEASE)" "record the release matching the previous private baseline"
        product_diff=$(git -C "$repo_root" diff --name-only "$previous_private" "$head_revision" -- \
            Cargo.toml Cargo.lock build.rs src ui packaging/macos/Info.plist.in packaging/macos/assets \
            packaging/macos/licenses packaging/macos/THIRD_PARTY_NOTICES.md)
        packaging_diff=$(git -C "$repo_root" diff --name-only "$previous_private" "$head_revision" -- \
            packaging/macos/entitlements.plist scripts/package-macos-app.sh scripts/build-macos-local.sh \
            scripts/finalize-release-manifest.sh scripts/record-signing-evidence.sh scripts/check-release.sh \
            scripts/check-release-e2e.sh docs/macos-release-guide.md)
        runtime_changed=false
        [ "$previous_runtime_tag/$previous_runtime_revision" = "$runtime_tag/$runtime_revision" ] || runtime_changed=true
        semver_impact=$(issue_value SEMVER_IMPACT)
        python3 - "$previous_version" "$app_version" "$semver_impact" "$classification" "$product_diff" "$runtime_changed" "$packaging_diff" <<'PY' || fail "classification and SemVer matching actual diff" mismatch "freeze the correct version/classification"
import sys
previous,current,impact,classification,product_diff,runtime_changed,packaging_diff=sys.argv[1:]
def version(value):
    parts=value.split("."); assert len(parts)==3 and all(x.isdigit() for x in parts); return tuple(map(int,parts))
old,new=version(previous),version(current)
if classification=="same-source-repackage":
    assert not product_diff and runtime_changed=="false" and new==old and impact=="none"
elif classification=="runtime-change":
    assert runtime_changed=="true" and impact in {"patch","minor","major"}
else:
    assert (product_diff or packaging_diff) and runtime_changed=="false" and impact in {"patch","minor","major"}
expected={"patch":(old[0],old[1],old[2]+1),"minor":(old[0],old[1]+1,0),"major":(old[0]+1,0,0)}.get(impact,old)
assert new==expected
PY
    else
        trailer_private=$(git -C "$repo_root" log -1 --format='%(trailers:key=Private-Revision,valueonly)')
        [ "$trailer_private" = "$expected_private_revision" ] || fail "public commit Private-Revision trailer $expected_private_revision" "${trailer_private:-missing}" "amend the public candidate commit metadata"
    fi
    if [ "$candidate" = public ]; then
        [ ! -e "$repo_root/.publicignore" ] || fail "public candidate without .publicignore" present "return to private source and apply .publicignore"
        [ -n "$publicignore" ] && [ -f "$publicignore" ] || fail "private .publicignore path" "${publicignore:-missing}" "pass --publicignore from the private repository"
        while IFS= read -r rule; do
            rule=${rule%%#*}; rule=${rule#/}; rule=${rule%/}
            [ -n "$rule" ] || continue
            excluded=$rule
            [ ! -e "$repo_root/$excluded" ] || fail "public candidate without $excluded" present "return to the private source and apply .publicignore"
        done < "$publicignore"
        rg -n '/Users/|BEGIN (RSA |OPENSSH |EC )?PRIVATE KEY|ghp_[A-Za-z0-9]+' "$repo_root/README.md" "$repo_root/docs" "$repo_root/packaging" && \
            fail "public files without credentials or private paths" found "remove private material from the public candidate"
    else
        rg -q '^/docs/public-release-runbook\.md$' "$repo_root/.publicignore" || fail "runbook excluded by .publicignore" missing "add the private runbook to .publicignore"
        snapshot_root=$(mktemp -d /private/tmp/bimyscribe-source-snapshot.XXXXXX)
        archive_root="$snapshot_root/archive"; public_root="$snapshot_root/public"
        mkdir -p "$archive_root" "$public_root"
        git -C "$repo_root" archive HEAD | tar -xf - -C "$archive_root"
        rsync -a --prune-empty-dirs --exclude-from="$repo_root/.publicignore" "$archive_root/" "$public_root/"
        "$repo_root/.agents/skills/publish-public/scripts/validate-snapshot.sh" "$public_root" "$repo_root/.publicignore" || {
            rm -rf "$snapshot_root"
            fail "filtered public snapshot validation" failed "remove private material or invalid references"
        }
        rm -rf "$snapshot_root"
    fi
    CARGO_TARGET_DIR="$build_root/cargo-target" cargo fmt --check || fail "cargo fmt" failed "format the private candidate"
    CARGO_TARGET_DIR="$build_root/cargo-target" cargo test --locked || fail "cargo test --locked" failed "fix tests in the private candidate"
    CARGO_TARGET_DIR="$build_root/cargo-target" cargo build --release --locked || fail "release build" failed "fix the release build"
    write_evidence "candidate=$candidate" "repository_revision=$head_revision" "private_revision=$expected_private_revision" \
        "issue_sha256=$(shasum -a 256 "$issue" | awk '{print $1}')"
    pass "source identity, documents, clean tree, format, tests and release build"
    ;;
runtime)
    [ -n "$runtime_repo" ] && [ -d "$runtime_repo/.git" ] || fail "Runtime clone" "${runtime_repo:-missing}" "pass --runtime-repo"
    [ -n "$download_dir" ] || fail "fresh Runtime download directory" missing "pass --download-dir"
    case "$download_dir" in /*) ;; *) fail "absolute Runtime download directory" "$download_dir" "pass a fresh absolute path" ;; esac
    [ ! -e "$download_dir" ] || fail "nonexistent Runtime download directory" exists "choose a fresh path"
    [ -n "$runtime_evidence" ] && [ -f "$runtime_evidence" ] || fail "Runtime fixed-sample evidence" "${runtime_evidence:-missing}" "pass --runtime-evidence"
    [ -z "$(git -C "$runtime_repo" status --porcelain --untracked-files=all)" ] || fail "clean Runtime clone" dirty "clean the Runtime clone"
    actual=$(git -C "$runtime_repo" rev-list -n 1 "$runtime_tag" 2>/dev/null || true)
    [ "$actual" = "$runtime_revision" ] || fail "$runtime_revision" "${actual:-missing}" "publish or fetch the pinned Runtime tag"
    manifest=$(git -C "$runtime_repo" show "$runtime_tag:bimyscribe-runtime.toml" 2>/dev/null || true)
    printf '%s\n' "$manifest" | rg -q '^backend = "native-uv"$' || fail "native-uv backend" missing "publish a native-uv Runtime"
    printf '%s\n' "$manifest" | rg -q "^contract_version = $runtime_contract$" || fail "contract $runtime_contract" mismatch "fix the pinned Runtime contract"
    for path in pyproject.toml .python-version uv.lock LICENSE bimyscribe-runtime.toml; do
        git -C "$runtime_repo" cat-file -e "$runtime_tag:$path" 2>/dev/null || fail "Runtime $path" missing "publish a complete Runtime tag"
    done
    remote=$(git -C "$runtime_repo" remote get-url origin 2>/dev/null || true)
    public_remote=$(printf '%s' "$remote" | sed -E 's#git@github.com:#https://github.com/#; s#\.git$##')
    remote_tag=$(git ls-remote "$public_remote.git" "refs/tags/$runtime_tag^{}" 2>/dev/null | awk 'NR==1 {print $1}')
    [ "$remote_tag" = "$runtime_revision" ] || fail "published Runtime $runtime_revision" "${remote_tag:-unavailable}" "publish and anonymously verify the Runtime tag"
    mkdir -p "$download_dir"
    archive="$download_dir/runtime.tar.gz"
    curl --fail --location --silent --show-error --output "$archive" "$public_remote/archive/refs/tags/$runtime_tag.tar.gz" || fail "anonymous Runtime source archive" unavailable "publish the Runtime tag publicly"
    tar -tzf "$archive" | rg -q '/bimyscribe-runtime.toml$' || fail "Runtime manifest in downloaded archive" missing "publish a complete Runtime tag"
    runtime_slug=${public_remote#https://github.com/}
    runtime_release=$(curl --fail --silent --show-error "https://api.github.com/repos/$runtime_slug/releases/tags/$runtime_tag" 2>/dev/null || true)
    [ -n "$runtime_release" ] || fail "public Runtime GitHub Release" unavailable "publish the Runtime Release"
    python3 - "$runtime_evidence" "$runtime_revision" "$runtime_contract" <<'PY' || fail "Runtime install/self-check/fixed-sample evidence" mismatch "revalidate the published Runtime with frozen uv"
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle: data=json.load(handle)
assert data.get("runtime_revision")==sys.argv[2]
assert data.get("backend")=="native-uv" and data.get("contract_version")==int(sys.argv[3])
for key in ("frozen_install", "self_check", "fixed_sample"):
    assert data.get(key, {}).get("status")=="passed"
assert data.get("target_platform")=="macos" and data.get("target_arch")=="arm64"
assert isinstance(data.get("assets"),list) and data["assets"]
PY
    while IFS=$'\t' read -r asset_name asset_sha asset_size; do
        asset_url=$(python3 -c 'import json,sys; data=json.load(sys.stdin); name=sys.argv[1]; print(next((x["browser_download_url"] for x in data["assets"] if x["name"]==name),""))' "$asset_name" <<<"$runtime_release")
        [ -n "$asset_url" ] || fail "Runtime Release asset $asset_name" missing "publish the frozen Runtime assets"
        curl --fail --location --silent --show-error --output "$download_dir/$asset_name" "$asset_url" || fail "anonymous Runtime asset $asset_name" unavailable "fix Runtime Release access"
        [ "$(shasum -a 256 "$download_dir/$asset_name" | awk '{print $1}')" = "$asset_sha" ] || fail "Runtime asset hash $asset_sha" mismatch "publish a new immutable Runtime tag"
        [ "$(stat -f %z "$download_dir/$asset_name")" = "$asset_size" ] || fail "Runtime asset size $asset_size" mismatch "publish the exact validated Runtime asset"
    done < <(python3 - "$runtime_evidence" <<'PY'
import json, sys
for item in json.load(open(sys.argv[1],encoding="utf-8"))["assets"]: print(item["name"],item["sha256"],item["size"],sep="\t")
PY
)
    write_evidence "archive_sha256=$(shasum -a 256 "$archive" | awk '{print $1}')" \
        "runtime_validation_sha256=$(shasum -a 256 "$runtime_evidence" | awk '{print $1}')"
    pass "anonymous Runtime tag/archive, revision, native-uv contract and validation evidence"
    ;;
app)
    [ -n "$app" ] && [ -d "$app/Contents" ] || fail "App bundle" "${app:-missing}" "pass --app"
    [ -n "$issue" ] && [ -f "$issue" ] || fail "release issue" "${issue:-missing}" "pass --issue"
    release_manifest="$app/Contents/Resources/release-manifest.json"
    [ -f "$release_manifest" ] || fail "release manifest" missing "reassemble the unsigned App"
    [ -n "$private_revision" ] || fail "source private revision" missing "pass --private-revision from private source evidence"
    printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || fail "full private revision" "$private_revision" "pass the frozen private candidate SHA"
    expected_public_revision=${public_revision:-$(git -C "$repo_root" rev-parse HEAD)}
    python3 - "$release_manifest" "$app_version" "$build_number" "$runtime_tag" "$runtime_revision" "$runtime_contract" "$uv_version" "$target_platform" "$target_arch" "$expected_public_revision" "$private_revision" <<'PY' || fail "release manifest matching fixed inputs and revisions" mismatch "reassemble from the frozen public/private revisions"
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle: data=json.load(handle)
expected={"app_version":sys.argv[2],"build_number":int(sys.argv[3]),"runtime_tag":sys.argv[4],
"runtime_revision":sys.argv[5],"runtime_backend":"native-uv","runtime_contract":int(sys.argv[6]),
"uv_version":sys.argv[7],"target_platform":sys.argv[8],"target_arch":sys.argv[9],
"public_revision":sys.argv[10],"private_revision":sys.argv[11]}
assert all(data.get(k)==v for k,v in expected.items())
PY
    for pair in \
        "binary_sha256:$app/Contents/MacOS/bimyscribe" \
        "uv_sha256:$app/Contents/Resources/bin/uv" \
        "runtime_manifest_sha256:$app/Contents/Resources/runtime/bimyscribe-runtime.toml"; do
        key=${pair%%:*}; path=${pair#*:}
        expected_hash=$(python3 - "$release_manifest" "$key" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle: print(json.load(handle).get(sys.argv[2], ""))
PY
)
        actual_hash=$(shasum -a 256 "$path" | awk '{print $1}')
        [ "$expected_hash" = "$actual_hash" ] || fail "$key $expected_hash" "$actual_hash" "reassemble the unsigned App"
    done
    plist_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")
    plist_build=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app/Contents/Info.plist")
    [ "$plist_version/$plist_build" = "$app_version/$build_number" ] || fail "$app_version/$build_number" "$plist_version/$plist_build" "reassemble Info.plist"
    file "$app/Contents/MacOS/bimyscribe" | rg -q 'Mach-O 64-bit executable arm64' || fail "arm64 App binary" wrong-architecture "use an arm64 release build"
    file "$app/Contents/Resources/bin/uv" | rg -q 'Mach-O 64-bit executable arm64' || fail "arm64 uv binary" wrong-architecture "use the pinned arm64 uv asset"
    "$app/Contents/MacOS/bimyscribe" --package-self-check || fail "package self-check" failed "return to App assembly"
    write_evidence "public_revision=$expected_public_revision" "private_revision=$private_revision" \
        "release_manifest_sha256=$(shasum -a 256 "$release_manifest" | awk '{print $1}')"
    pass "App identity, manifest, bundled native-uv and package self-check"
    ;;
dmg)
    [ -n "$issue" ] && [ -f "$issue" ] || fail "release issue" "${issue:-missing}" "pass --issue"
    [ -n "$signing_evidence" ] && [ -f "$signing_evidence" ] || fail "independent signing/notarization evidence" "${signing_evidence:-missing}" "run record-signing-evidence.sh"
    [ -n "$dmg" ] && [ -f "$dmg" ] || fail "DMG" "${dmg:-missing}" "pass --dmg"
    expected_name="BiMyScribe-v$app_version-macos-arm64.dmg"
    [ "$(basename "$dmg")" = "$expected_name" ] || fail "$expected_name" "$(basename "$dmg")" "recreate the DMG without renaming"
    [ -n "$sha_file" ] && [ -f "$sha_file" ] || fail "SHA-256 file" "${sha_file:-missing}" "pass --sha256-file"
    (cd "$(dirname "$dmg")" && shasum -a 256 -c "$(basename "$sha_file")") || fail "matching SHA-256" mismatch "freeze and checksum the final DMG"
    codesign --verify --strict --verbose=2 "$dmg" || fail "valid DMG signature" invalid "return to signing"
    xcrun stapler validate "$dmg" || fail "valid notarization staple" invalid "return to notarization"
    spctl --assess --type open --context context:primary-signature "$dmg" || fail "Gatekeeper-approved DMG" rejected "return to signing/notarization"
    mount_root=$(mktemp -d /private/tmp/bimyscribe-dmg-check.XXXXXX)
    cleanup_mount() { hdiutil detach "$mount_root" >/dev/null 2>&1 || true; rmdir "$mount_root" >/dev/null 2>&1 || true; }
    trap cleanup_mount EXIT
    hdiutil attach -nobrowse -readonly -mountpoint "$mount_root" "$dmg" >/dev/null || fail "read-only DMG mount" failed "recreate the DMG"
    top_level=$(python3 - "$mount_root" <<'PY'
import json, os, sys
print(json.dumps(sorted(os.listdir(sys.argv[1]))))
PY
)
    [ "$top_level" = '["BiMyScribe.app"]' ] || fail "DMG top level containing only BiMyScribe.app" "$top_level" "recreate from a dedicated clean staging directory"
    mounted_app="$mount_root/BiMyScribe.app"
    [ -d "$mounted_app" ] || fail "mounted BiMyScribe.app" missing "recreate the DMG layout"
    codesign --verify --deep --strict --verbose=2 "$mounted_app" || fail "mounted App signature" invalid "return to App signing"
    spctl --assess --type execute "$mounted_app" || fail "Gatekeeper-approved mounted App" rejected "return to signing/notarization"
    mounted_identity=$(codesign -dvvv "$mounted_app" 2>&1 | sed -n 's/^Authority=//p' | head -n 1)
    mounted_tree_hash=$(python3 - "$mounted_app" <<'PY'
import hashlib, os, stat, sys
root=os.path.realpath(sys.argv[1]); digest=hashlib.sha256()
for base,dirs,files in os.walk(root):
    dirs.sort(); files.sort()
    for name in files:
        path=os.path.join(base,name); digest.update(os.path.relpath(path,root).encode()); digest.update(b"\0")
        digest.update(oct(stat.S_IMODE(os.lstat(path).st_mode)).encode()); digest.update(b"\0")
        if os.path.islink(path): digest.update(os.readlink(path).encode())
        else:
            with open(path,"rb") as handle:
                for chunk in iter(lambda:handle.read(1024*1024),b""): digest.update(chunk)
print(digest.hexdigest())
PY
)
    python3 - "$signing_evidence" "$mounted_tree_hash" "$mounted_identity" "$(shasum -a 256 "$dmg" | awk '{print $1}')" <<'PY' || fail "signing/notarization evidence matching final mounted bytes" mismatch "regenerate signing evidence from the frozen App and DMG"
import json, sys
with open(sys.argv[1],encoding="utf-8") as handle: data=json.load(handle)
assert data.get("app_tree_sha256")==sys.argv[2] and data.get("signing_identity")==sys.argv[3]
assert data.get("dmg_sha256")==sys.argv[4] and data.get("notarization_request_id")
assert data.get("notarization_status")=="accepted" and data.get("staple")=="passed" and data.get("gatekeeper")=="passed"
PY
    "$mounted_app/Contents/MacOS/bimyscribe" --package-self-check || fail "mounted App package self-check" failed "return to App assembly"
    manifest="$mounted_app/Contents/Resources/release-manifest.json"
    [ -n "$private_revision" ] || fail "source private revision" missing "pass --private-revision from private source evidence"
    printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || fail "full private revision" "$private_revision" "pass the frozen private candidate SHA"
    expected_public_revision=${public_revision:-$(git -C "$repo_root" rev-parse HEAD)}
    python3 - "$manifest" "$app_version" "$build_number" "$runtime_tag" "$runtime_revision" "$uv_version" "$target_platform" "$target_arch" "$expected_public_revision" "$private_revision" <<'PY' || fail "mounted App identity" mismatch "rebuild, sign and notarize the App"
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle: data=json.load(handle)
expected={"app_version":sys.argv[2],"build_number":int(sys.argv[3]),"runtime_tag":sys.argv[4],
"runtime_revision":sys.argv[5],"runtime_backend":"native-uv","uv_version":sys.argv[6],
"target_platform":sys.argv[7],"target_arch":sys.argv[8],"public_revision":sys.argv[9],"private_revision":sys.argv[10]}
assert all(data.get(key)==value for key,value in expected.items())
PY
    [ -n "$release_check_root" ] && [ -n "$e2e_evidence" ] || fail "release-check root and E2E evidence path" missing "pass --release-check-root and --e2e-evidence"
    [ "${#protected_paths[@]}" -ge 2 ] || fail "development and production protected paths" "${#protected_paths[@]} supplied" "pass both state roots with --protected-path"
    e2e_args=(--app "$mounted_app" --release-check-root "$release_check_root" --evidence "$e2e_evidence")
    for path in "${protected_paths[@]}"; do e2e_args+=(--protected-path "$path"); done
    "$repo_root/scripts/check-release-e2e.sh" "${e2e_args[@]}" || fail "final fixed-fixture native-uv E2E" failed "return to the Runtime, source or App gate"
    dmg_hash=$(shasum -a 256 "$dmg" | awk '{print $1}')
    e2e_hash=$(shasum -a 256 "$e2e_evidence" | awk '{print $1}')
    protected_joined=$(IFS='|'; printf '%s' "${protected_paths[*]}")
    write_evidence "dmg_sha256=$dmg_hash" "dmg_size=$(stat -f %z "$dmg")" "e2e_sha256=$e2e_hash" \
        "protected_paths=$protected_joined" "public_revision=$expected_public_revision" "private_revision=$private_revision" \
        "signing_evidence_sha256=$(shasum -a 256 "$signing_evidence" | awk '{print $1}')"
    cleanup_mount; trap - EXIT
    pass "DMG bytes, trust, mounted App identity and package self-check"
    ;;
approve)
    [ -n "$issue" ] && [ -f "$issue" ] || fail "release issue" "${issue:-missing}" "pass --issue"
    [ -n "$dmg" ] && [ -f "$dmg" ] || fail "frozen DMG" "${dmg:-missing}" "pass --dmg"
    [ -n "$e2e_evidence" ] && [ -f "$e2e_evidence" ] || fail "final E2E evidence" "${e2e_evidence:-missing}" "pass --e2e-evidence"
    [ -n "$signing_evidence" ] && [ -f "$signing_evidence" ] || fail "signing/notarization evidence" "${signing_evidence:-missing}" "pass --signing-evidence"
    [ -n "$evidence_out" ] || fail "approval evidence output" missing "pass --evidence-out"
    case "$evidence_out" in /*) ;; *) fail "absolute approval evidence output" "$evidence_out" "use an approved absolute path" ;; esac
    [ ! -e "$evidence_out" ] || fail "nonexistent approval evidence output" exists "choose a new path"
    for evidence in "$private_source_evidence" "$public_source_evidence" "$runtime_gate_evidence" "$app_evidence" "$dmg_evidence"; do
        [ -n "$evidence" ] && [ -f "$evidence" ] || fail "all source/runtime/app/dmg evidence files" "${evidence:-missing}" "pass every stage evidence path"
    done
    issue_value() { sed -n "s/^$1:[[:space:]]*//p" "$issue" | head -n 1; }
    for number in $(seq 0 11); do
        actual_status=$(issue_value "STEP_${number}_STATUS")
        [ "$actual_status" = passed ] || fail "STEP_${number}_STATUS=passed" "${actual_status:-missing}" "complete the earliest missing release gate"
    done
    [ "$(issue_value UNEXPLAINED_SKIPS)" = none ] || fail "UNEXPLAINED_SKIPS=none" "$(issue_value UNEXPLAINED_SKIPS)" "resolve every skipped gate"
    authorized=$(issue_value AUTHORIZED_ACTIONS)
    for action in tag push release; do
        printf '%s' "$authorized" | rg -q "(^|,)[[:space:]]*$action([[:space:]]*,|$)" || fail "authorization for $action" "$authorized" "obtain explicit Step 12 authorization"
    done
    actual_sha=$(shasum -a 256 "$dmg" | awk '{print $1}')
    [ "$(issue_value DMG_SHA256)" = "$actual_sha" ] || fail "recorded DMG SHA-256 $actual_sha" "$(issue_value DMG_SHA256)" "freeze the DMG and update the issue"
    e2e_sha=$(shasum -a 256 "$e2e_evidence" | awk '{print $1}')
    [ "$(issue_value E2E_EVIDENCE_SHA256)" = "$e2e_sha" ] || fail "recorded E2E evidence $e2e_sha" "$(issue_value E2E_EVIDENCE_SHA256)" "record the final E2E evidence hash"
    expected_public_revision=$(git -C "$repo_root" rev-parse HEAD)
    [ -n "$private_revision" ] || fail "source private revision" missing "pass --private-revision from private source evidence"
    printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || fail "full private revision" "$private_revision" "pass the frozen private candidate SHA"
    expected_private_revision=$private_revision
    approval_protected_paths=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["protected_paths"])' "$dmg_evidence")
    python3 - "$private_source_evidence" "$public_source_evidence" "$runtime_gate_evidence" "$app_evidence" "$dmg_evidence" \
        "$config_sha256" "$expected_private_revision" "$expected_public_revision" "$actual_sha" "$e2e_sha" "$signing_evidence" <<'PY' || fail "cross-checked stage evidence chain" mismatch "rerun the earliest invalidated gate"
import json, sys
paths=sys.argv[1:6]; config_sha,private_rev,public_rev,dmg_sha,e2e_sha,signing_path=sys.argv[6:]
items=[]
for path in paths:
    with open(path,encoding="utf-8") as handle: items.append(json.load(handle))
assert [x.get("stage") for x in items]==["source","source","runtime","app","dmg"]
assert items[0].get("candidate")=="private" and items[0].get("repository_revision")==private_rev
assert items[1].get("candidate")=="public" and items[1].get("repository_revision")==public_rev and items[1].get("private_revision")==private_rev
assert all(x.get("status")=="passed" and x.get("inputs",{}).get("release_config_sha256")==config_sha for x in items)
assert items[3].get("public_revision")==public_rev and items[3].get("private_revision")==private_rev
assert items[4].get("public_revision")==public_rev and items[4].get("private_revision")==private_rev
assert items[4].get("dmg_sha256")==dmg_sha and items[4].get("e2e_sha256")==e2e_sha
assert items[4].get("protected_paths") and "|" in items[4]["protected_paths"]
import hashlib
assert items[4].get("signing_evidence_sha256")==hashlib.sha256(open(signing_path,"rb").read()).hexdigest()
PY
    mkdir -p "$(dirname "$evidence_out")"
    python3 - "$evidence_out" "$app_version" "$build_number" "$runtime_tag" "$runtime_revision" "$actual_sha" "$(stat -f %z "$dmg")" "$expected_public_revision" \
        "$expected_private_revision" "$config_sha256" "$(shasum -a 256 "$private_source_evidence" | awk '{print $1}')" \
        "$(shasum -a 256 "$public_source_evidence" | awk '{print $1}')" "$(shasum -a 256 "$runtime_gate_evidence" | awk '{print $1}')" \
        "$(shasum -a 256 "$app_evidence" | awk '{print $1}')" "$(shasum -a 256 "$dmg_evidence" | awk '{print $1}')" "$approval_protected_paths" \
        "$(shasum -a 256 "$signing_evidence" | awk '{print $1}')" <<'PY'
import json, sys
path=sys.argv[1]
keys=("app_version","build_number","runtime_tag","runtime_revision","dmg_sha256","dmg_size","public_revision")
values=sys.argv[2:9]; values[1]=int(values[1]); values[5]=int(values[5]); data=dict(zip(keys,values))
data["private_revision"]=sys.argv[9]; data["release_config_sha256"]=sys.argv[10]
data["evidence_sha256"]={key:value for key,value in zip(("private_source","public_source","runtime","app","dmg"),sys.argv[11:16])}
data["protected_paths"]=sys.argv[16]
data["signing_evidence_sha256"]=sys.argv[17]
with open(path,"x",encoding="utf-8") as handle: json.dump(data,handle,sort_keys=True); handle.write("\n")
print(json.dumps(data,sort_keys=True))
PY
    ;;
published)
    [ -n "$repo_slug" ] && [ -n "$tag" ] && [ -n "$public_revision" ] && [ -n "$download_dir" ] || fail "repo, tag, public revision and download dir" missing "pass all published-stage inputs"
    [ -n "$approval_file" ] && [ -f "$approval_file" ] || fail "Step 12 approval result" "${approval_file:-missing}" "run approve and save its JSON output"
    [ -n "$frozen_sha256" ] || fail "pre-publication frozen SHA-256" missing "pass --frozen-sha256 from Step 12"
    [ -n "$release_check_root" ] && [ -n "$e2e_evidence" ] || fail "fresh published-check root and evidence path" missing "pass post-download verification paths"
    [ -n "$postdownload_dmg_evidence" ] || fail "post-download DMG evidence path" missing "pass --postdownload-dmg-evidence"
    [ -n "$issue" ] && [ -f "$issue" ] || fail "release issue" "${issue:-missing}" "pass --issue"
    [ "${#protected_paths[@]}" -ge 2 ] || fail "development and production protected paths" "${#protected_paths[@]} supplied" "pass both state roots"
    for evidence in "$private_source_evidence" "$public_source_evidence" "$runtime_gate_evidence" "$app_evidence" "$dmg_evidence" "$signing_evidence"; do
        [ -n "$evidence" ] && [ -f "$evidence" ] || fail "approved upstream evidence files" "${evidence:-missing}" "pass the exact Step 12 evidence paths"
    done
    [ -n "$private_revision" ] || fail "source private revision" missing "pass --private-revision from approved evidence"
    printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || fail "full private revision" "$private_revision" "pass the frozen private candidate SHA"
    python3 - "$approval_file" "$app_version" "$build_number" "$runtime_tag" "$runtime_revision" "$frozen_sha256" "$public_revision" "$config_sha256" \
        "$private_revision" "$private_source_evidence" "$public_source_evidence" "$runtime_gate_evidence" "$app_evidence" "$dmg_evidence" "$signing_evidence" <<'PY' || fail "approval and upstream evidence matching published inputs" mismatch "return to Step 12 approval"
import json, sys
import hashlib
with open(sys.argv[1], encoding="utf-8") as handle: data=json.load(handle)
expected={"app_version":sys.argv[2],"build_number":int(sys.argv[3]),"runtime_tag":sys.argv[4],
          "runtime_revision":sys.argv[5],"dmg_sha256":sys.argv[6],"public_revision":sys.argv[7],"private_revision":sys.argv[9]}
assert all(data.get(k)==v for k,v in expected.items())
assert data.get("release_config_sha256")==sys.argv[8]
paths=sys.argv[10:15]; names=("private_source","public_source","runtime","app","dmg")
actual={name:hashlib.sha256(open(path,"rb").read()).hexdigest() for name,path in zip(names,paths)}
assert data.get("evidence_sha256")==actual
assert data.get("signing_evidence_sha256")==hashlib.sha256(open(sys.argv[15],"rb").read()).hexdigest()
PY
    approved_protected=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["protected_paths"])' "$approval_file")
    supplied_protected=$(IFS='|'; printf '%s' "${protected_paths[*]}")
    [ "$approved_protected" = "$supplied_protected" ] || fail "approved protected path set" "$supplied_protected" "use the exact Step 12 protected paths"
    case "$download_dir" in /*) ;; *) fail "absolute fresh download dir" "$download_dir" "choose a new absolute directory" ;; esac
    [ ! -e "$download_dir" ] || fail "nonexistent download directory" exists "choose a fresh path"
    release_api="https://api.github.com/repos/$repo_slug/releases/tags/$tag"
    release_json=$(curl --fail --silent --show-error -H 'Accept: application/vnd.github+json' "$release_api" 2>/dev/null || true)
    [ -n "$release_json" ] || fail "public Release API" unavailable "publish the GitHub Release"
    remote_revision=$(git ls-remote "https://github.com/$repo_slug.git" "refs/tags/$tag^{}" 2>/dev/null | awk 'NR==1 {print $1}')
    [ -n "$remote_revision" ] || fail "annotated tag $tag" lightweight-or-missing "create an annotated immutable tag"
    [ "$remote_revision" = "$public_revision" ] || fail "$public_revision" "${remote_revision:-missing}" "fix the tag target; never move a public tag"
    mkdir -p "$download_dir"
    expected_dmg="BiMyScribe-v$app_version-macos-arm64.dmg"
    for asset in "$expected_dmg" "$expected_dmg.sha256"; do
        url=$(python3 -c 'import json,sys; data=json.load(sys.stdin); name=sys.argv[1]; print(next((x["browser_download_url"] for x in data["assets"] if x["name"]==name), ""))' "$asset" <<<"$release_json")
        [ -n "$url" ] || fail "public asset $asset" missing "upload the exact frozen assets"
        curl --fail --location --silent --show-error --output "$download_dir/$asset" "$url" || fail "anonymous download $asset" unavailable "fix public asset access"
    done
    published_title=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("name", ""))' <<<"$release_json")
    published_draft=$(python3 -c 'import json,sys; print(str(json.load(sys.stdin).get("draft", True)).lower())' <<<"$release_json")
    published_prerelease=$(python3 -c 'import json,sys; print(str(json.load(sys.stdin).get("prerelease", True)).lower())' <<<"$release_json")
    [ "$published_title" = "BiMyScribe v$app_version" ] || fail "BiMyScribe v$app_version" "$published_title" "correct the Release title"
    [ "$published_draft/$published_prerelease" = "false/false" ] || fail "non-draft non-prerelease" "$published_draft/$published_prerelease" "publish the final Release correctly"
    published_body=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("body", ""), end="")' <<<"$release_json")
    [ "$published_body" = "$(cat "$repo_root/$notes_path")" ] || fail "versioned Release Notes body" different "create the Release from $notes_path"
    expected_size=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["dmg_size"])' "$approval_file")
    actual_size=$(python3 -c 'import json,sys; data=json.load(sys.stdin); name=sys.argv[1]; print(next(x["size"] for x in data["assets"] if x["name"]==name))' "$expected_dmg" <<<"$release_json")
    [ "$actual_size" = "$expected_size" ] || fail "asset size $expected_size" "$actual_size" "publish the exact frozen DMG"
    downloaded_dmg=$(find "$download_dir" -maxdepth 1 -name '*.dmg' -type f | head -n 1)
    downloaded_sha=$(find "$download_dir" -maxdepth 1 -name '*.sha256' -type f | head -n 1)
    downloaded_hash=$(shasum -a 256 "$downloaded_dmg" | awk '{print $1}')
    [ "$downloaded_hash" = "$frozen_sha256" ] || fail "published bytes $frozen_sha256" "$downloaded_hash" "publish a new corrective version; never replace public bytes"
    nested_args=(dmg --dmg "$downloaded_dmg" --sha256-file "$downloaded_sha" --release-check-root "$release_check_root" \
        --e2e-evidence "$e2e_evidence" --issue "$issue" --public-revision "$public_revision" --evidence-out "$postdownload_dmg_evidence" \
        --private-revision "$private_revision" --signing-evidence "$signing_evidence")
    for path in "${protected_paths[@]}"; do nested_args+=(--protected-path "$path"); done
    "$0" "${nested_args[@]}" || fail "downloaded DMG gate" failed "publish a new corrective version"
    source_archive="$download_dir/public-source.tar.gz"
    curl --fail --location --silent --show-error --output "$source_archive" "https://github.com/$repo_slug/archive/refs/tags/$tag.tar.gz" || fail "anonymous public source snapshot" unavailable "fix public tag access"
    source_root="$download_dir/source"
    mkdir -p "$source_root"; tar -xzf "$source_archive" --strip-components=1 -C "$source_root"
    [ -f "$source_root/README.md" ] && [ -f "$source_root/CHANGELOG.md" ] && [ -f "$source_root/$notes_path" ] || fail "tagged public README/Changelog/Release Notes" missing "publish complete public documentation"
    cmp -s "$source_root/$notes_path" "$repo_root/$notes_path" || fail "tagged versioned Release Notes" different "publish the reviewed notes"
    rg -q "v$app_version" "$source_root/README.md" || fail "tagged README installation reference v$app_version" missing "update public installation documentation"
    rg -q "## $app_version" "$source_root/CHANGELOG.md" || fail "tagged Changelog entry $app_version" missing "publish CHANGELOG.md"
    rg -o 'https://github\.com/[^ )]+' "$source_root/README.md" | sort -u | while IFS= read -r link; do
        curl --fail --location --silent --show-error --range 0-0 --output /dev/null "$link" || fail "anonymous README link" "$link unavailable" "repair public installation links"
    done
    write_evidence "public_revision=$public_revision" "downloaded_dmg_sha256=$downloaded_hash" \
        "postdownload_dmg_evidence_sha256=$(shasum -a 256 "$postdownload_dmg_evidence" | awk '{print $1}')"
    pass "remote annotated tag, public docs and freshly downloaded identical asset"
    ;;
*) usage ;;
esac
