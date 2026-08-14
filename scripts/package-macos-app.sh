#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s --output-dir DIR --private-revision SHA [--runtime-repo DIR] [--uv-bin FILE]\n' "$0"
}

output_dir=""
runtime_repo=""
uv_bin=""
private_revision=""
requested_build_number=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --output-dir) output_dir=${2:?}; shift 2 ;;
        --runtime-repo) runtime_repo=${2:?}; shift 2 ;;
        --uv-bin) uv_bin=${2:?}; shift 2 ;;
        --private-revision) private_revision=${2:?}; shift 2 ;;
        --build-number) requested_build_number=${2:?}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
release_config="$repo_root/packaging/release.toml"
[ -f "$release_config" ] || { printf 'Release config is missing: %s\n' "$release_config" >&2; exit 1; }
release_values=$(python3 - "$release_config" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as handle:
    data = tomllib.load(handle)
for key in ("build_number", "runtime_tag", "runtime_revision", "runtime_contract", "uv_version", "target_platform", "target_arch"):
    print(data[key])
PY
)
build_number=$(printf '%s\n' "$release_values" | sed -n '1p')
runtime_tag=$(printf '%s\n' "$release_values" | sed -n '2p')
runtime_revision=$(printf '%s\n' "$release_values" | sed -n '3p')
runtime_contract=$(printf '%s\n' "$release_values" | sed -n '4p')
expected_uv_version=$(printf '%s\n' "$release_values" | sed -n '5p')
target_platform=$(printf '%s\n' "$release_values" | sed -n '6p')
target_arch=$(printf '%s\n' "$release_values" | sed -n '7p')
runtime_repo=${runtime_repo:-"$(dirname "$repo_root")/bimyscribe-funasr-runtime-release"}
uv_bin=${uv_bin:-"$(command -v uv || true)"}

[ -n "$output_dir" ] || { usage >&2; exit 2; }
[ -n "$private_revision" ] || { printf -- '--private-revision is required.\n' >&2; exit 2; }
printf '%s' "$private_revision" | rg -q '^[0-9a-f]{40}$' || { printf 'Private revision must be a full commit SHA.\n' >&2; exit 2; }
[ -z "$requested_build_number" ] || [ "$requested_build_number" = "$build_number" ] || {
    printf 'Build number override rejected: config=%s argument=%s. Update packaging/release.toml.\n' "$build_number" "$requested_build_number" >&2
    exit 1
}
[ "$target_platform" = "macos" ] && [ "$target_arch" = "arm64" ] || {
    printf 'Unsupported release target: %s/%s.\n' "$target_platform" "$target_arch" >&2; exit 1;
}
case "$output_dir" in /*) ;; *) printf 'Output directory must be absolute.\n' >&2; exit 2 ;; esac
[ -d "$runtime_repo/.git" ] || { printf 'Runtime repository is invalid: %s\n' "$runtime_repo" >&2; exit 1; }
[ -x "$uv_bin" ] || { printf 'uv executable is unavailable: %s\n' "$uv_bin" >&2; exit 1; }
[ -n "${CARGO_TARGET_DIR:-}" ] || {
    printf 'CARGO_TARGET_DIR must point to the approved build-data location.\n' >&2
    exit 1
}
case "$CARGO_TARGET_DIR" in /*) ;; *) printf 'CARGO_TARGET_DIR must be absolute.\n' >&2; exit 2 ;; esac

[ -z "$(git -C "$repo_root" status --porcelain --untracked-files=all)" ] || {
    printf 'BiMyScribe source repository must be clean.\n' >&2
    exit 1
}
[ -z "$(git -C "$runtime_repo" status --porcelain --untracked-files=all)" ] || {
    printf 'Runtime repository must be clean.\n' >&2
    exit 1
}
git -C "$runtime_repo" rev-parse -q --verify "refs/tags/$runtime_tag" >/dev/null || {
    printf 'Runtime tag is unavailable: %s\n' "$runtime_tag" >&2
    exit 1
}
actual_runtime_revision=$(git -C "$runtime_repo" rev-list -n 1 "$runtime_tag")
[ "$actual_runtime_revision" = "$runtime_revision" ] || {
    printf 'Runtime revision mismatch: expected %s, found %s.\n' "$runtime_revision" "$actual_runtime_revision" >&2; exit 1;
}
runtime_manifest=$(git -C "$runtime_repo" show "$runtime_tag:bimyscribe-runtime.toml")
printf '%s\n' "$runtime_manifest" | rg -q '^backend = "native-uv"$' || {
    printf 'Bundled Runtime backend must be native-uv.\n' >&2; exit 1;
}
printf '%s\n' "$runtime_manifest" | rg -q "^contract_version = $runtime_contract$" || {
    printf 'Runtime contract mismatch: expected %s.\n' "$runtime_contract" >&2; exit 1;
}
for required_runtime_file in pyproject.toml .python-version uv.lock LICENSE bimyscribe-runtime.toml; do
    git -C "$runtime_repo" cat-file -e "$runtime_tag:$required_runtime_file" 2>/dev/null || {
        printf 'Runtime tag is incomplete; missing %s.\n' "$required_runtime_file" >&2
        exit 1
    }
done

actual_uv_version=$($uv_bin --version | awk '{print $2}')
[ "$actual_uv_version" = "$expected_uv_version" ] || {
    printf 'Expected uv %s, found %s.\n' "$expected_uv_version" "$actual_uv_version" >&2
    exit 1
}
file "$uv_bin" | rg -q 'Mach-O 64-bit executable arm64' || {
    printf 'uv must be an arm64 macOS executable.\n' >&2
    exit 1
}

app_version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "$repo_root/Cargo.toml" | head -n 1)
[ -n "$app_version" ] || { printf 'Cannot read application version.\n' >&2; exit 1; }
case "$build_number" in *[!0-9]*|'') printf 'Build number must be a positive integer.\n' >&2; exit 2 ;; esac
[ "$build_number" -gt 0 ] || { printf 'Build number must be greater than zero.\n' >&2; exit 2; }
public_revision=$(git -C "$repo_root" rev-parse HEAD)

app="$output_dir/BiMyScribe.app"
[ ! -e "$app" ] || { printf 'Refusing to replace existing bundle: %s\n' "$app" >&2; exit 1; }
mkdir -p "$output_dir" "$CARGO_TARGET_DIR"

cargo build --manifest-path "$repo_root/Cargo.toml" --release --locked --bin bimyscribe
binary="$CARGO_TARGET_DIR/release/bimyscribe"
[ -x "$binary" ] || { printf 'Release binary was not produced: %s\n' "$binary" >&2; exit 1; }
file "$binary" | rg -q 'Mach-O 64-bit executable arm64' || {
    printf 'BiMyScribe release binary is not arm64.\n' >&2
    exit 1
}

contents="$app/Contents"
resources="$contents/Resources"
mkdir -p "$contents/MacOS" "$resources/bin" "$resources/runtime" "$resources/licenses"
cp "$binary" "$contents/MacOS/bimyscribe"
cp "$uv_bin" "$resources/bin/uv"
cp "$repo_root/packaging/macos/assets/AppIcon.icns" "$resources/AppIcon.icns"
chmod 755 "$contents/MacOS/bimyscribe" "$resources/bin/uv"
git -C "$runtime_repo" archive "$runtime_tag" | tar -xf - -C "$resources/runtime"

binary_sha256=$(shasum -a 256 "$contents/MacOS/bimyscribe" | awk '{print $1}')
uv_sha256=$(shasum -a 256 "$resources/bin/uv" | awk '{print $1}')
runtime_manifest_sha256=$(shasum -a 256 "$resources/runtime/bimyscribe-runtime.toml" | awk '{print $1}')

python3 - "$resources/release-manifest.json" "$app_version" "$build_number" "$public_revision" \
    "$private_revision" "$runtime_tag" "$runtime_revision" "$runtime_contract" \
    "$expected_uv_version" "$target_platform" "$target_arch" "$binary_sha256" "$uv_sha256" \
    "$runtime_manifest_sha256" <<'PY'
import json, sys
keys = ("app_version", "build_number", "public_revision", "private_revision", "runtime_tag",
        "runtime_revision", "runtime_contract", "uv_version", "target_platform", "target_arch",
        "binary_sha256", "uv_sha256", "runtime_manifest_sha256")
values = sys.argv[2:]
values[1] = int(values[1]); values[6] = int(values[6])
data = dict(zip(keys, values)); data["runtime_backend"] = "native-uv"
with open(sys.argv[1], "x", encoding="utf-8") as handle:
    json.dump(data, handle, ensure_ascii=False, indent=2, sort_keys=True)
    handle.write("\n")
PY

sed -e "s/@APP_VERSION@/$app_version/g" -e "s/@BUILD_NUMBER@/$build_number/g" \
    "$repo_root/packaging/macos/Info.plist.in" > "$contents/Info.plist"
cp "$repo_root/LICENSE" "$resources/licenses/BiMyScribe-MIT.txt"
cp "$repo_root/packaging/macos/THIRD_PARTY_NOTICES.md" "$resources/licenses/THIRD_PARTY_NOTICES.md"
cp "$repo_root/packaging/macos/licenses/UV-LICENSE-APACHE" "$resources/licenses/UV-LICENSE-APACHE"
cp "$repo_root/packaging/macos/licenses/UV-LICENSE-MIT" "$resources/licenses/UV-LICENSE-MIT"

plutil -lint "$contents/Info.plist" >/dev/null
test -f "$resources/runtime/bimyscribe-runtime.toml"
test -f "$resources/runtime/uv.lock"
test -f "$resources/AppIcon.icns"
test -x "$resources/bin/uv"
test -f "$resources/release-manifest.json"
"$contents/MacOS/bimyscribe" --package-self-check

printf 'Created unsigned application bundle:\n%s\n' "$app"
printf 'App version: %s (%s)\nRuntime: %s\nuv: %s\n' \
    "$app_version" "$build_number" "$runtime_tag" "$actual_uv_version"
