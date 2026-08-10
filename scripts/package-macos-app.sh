#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s --output-dir DIR [--runtime-repo DIR] [--uv-bin FILE] [--build-number N]\n' "$0"
}

output_dir=""
runtime_repo=""
uv_bin=""
build_number="1"
runtime_tag="${BIMYSCRIBE_RUNTIME_TAG:-v1.0.0}"
expected_uv_version="${BIMYSCRIBE_UV_VERSION:-0.11.23}"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --output-dir) output_dir=${2:?}; shift 2 ;;
        --runtime-repo) runtime_repo=${2:?}; shift 2 ;;
        --uv-bin) uv_bin=${2:?}; shift 2 ;;
        --build-number) build_number=${2:?}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
runtime_repo=${runtime_repo:-"$(dirname "$repo_root")/bimyscribe-funasr-runtime-release"}
uv_bin=${uv_bin:-"$(command -v uv || true)"}

[ -n "$output_dir" ] || { usage >&2; exit 2; }
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
chmod 755 "$contents/MacOS/bimyscribe" "$resources/bin/uv"
git -C "$runtime_repo" archive "$runtime_tag" | tar -xf - -C "$resources/runtime"

sed -e "s/@APP_VERSION@/$app_version/g" -e "s/@BUILD_NUMBER@/$build_number/g" \
    "$repo_root/packaging/macos/Info.plist.in" > "$contents/Info.plist"
cp "$repo_root/LICENSE" "$resources/licenses/BiMyScribe-MIT.txt"
cp "$repo_root/packaging/macos/THIRD_PARTY_NOTICES.md" "$resources/licenses/THIRD_PARTY_NOTICES.md"
cp "$repo_root/packaging/macos/licenses/UV-LICENSE-APACHE" "$resources/licenses/UV-LICENSE-APACHE"
cp "$repo_root/packaging/macos/licenses/UV-LICENSE-MIT" "$resources/licenses/UV-LICENSE-MIT"

plutil -lint "$contents/Info.plist" >/dev/null
test -f "$resources/runtime/bimyscribe-runtime.toml"
test -f "$resources/runtime/uv.lock"
test -x "$resources/bin/uv"
"$contents/MacOS/bimyscribe" --package-self-check

printf 'Created unsigned application bundle:\n%s\n' "$app"
printf 'App version: %s (%s)\nRuntime: %s\nuv: %s\n' \
    "$app_version" "$build_number" "$runtime_tag" "$actual_uv_version"
