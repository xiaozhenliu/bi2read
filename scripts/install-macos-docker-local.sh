#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/install-macos-docker-local.sh \
    --build-root /Volumes/<external-disk>/... \
    --runtime-project /absolute/path/to/docker-runtime \
    --runtime-data-dir /Volumes/<external-disk>/...

Options:
  --install-app PATH   Installation target (default: /Applications/bi2read.app)
  --no-launch          Install and verify without launching the App
  -h, --help           Show this help

This maintenance-machine installer creates an ad-hoc signed, Docker-only App.
It never downloads or bundles native-uv, Python, model weights, or uv.
EOF
}

build_root=""
runtime_project=""
runtime_data_dir=""
install_app="/Applications/bi2read.app"
launch=true

while [ "$#" -gt 0 ]; do
    case "$1" in
        --build-root) build_root=${2:?}; shift 2 ;;
        --runtime-project) runtime_project=${2:?}; shift 2 ;;
        --runtime-data-dir) runtime_data_dir=${2:?}; shift 2 ;;
        --install-app) install_app=${2:?}; shift 2 ;;
        --no-launch) launch=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
for command_name in codesign ditto file git open plutil python3 rg shasum; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'Required command is unavailable: %s\n' "$command_name" >&2
        exit 1
    }
done
cargo_bin=$(command -v cargo || true)
if [ -z "$cargo_bin" ] && [ -x "$HOME/.cargo/bin/cargo" ]; then
    cargo_bin="$HOME/.cargo/bin/cargo"
fi
[ -n "$cargo_bin" ] || { printf 'Required command is unavailable: cargo\n' >&2; exit 1; }

for value_name in build_root runtime_project runtime_data_dir install_app; do
    value=${!value_name}
    case "$value" in
        /*) ;;
        *) printf '%s must be an absolute path.\n' "$value_name" >&2; exit 2 ;;
    esac
done
case "$build_root" in
    /Volumes/*) ;;
    *) printf 'build_root must be on an external volume under /Volumes.\n' >&2; exit 2 ;;
esac
case "$runtime_data_dir" in
    /Volumes/*) ;;
    *) printf 'runtime_data_dir must be on an external volume under /Volumes.\n' >&2; exit 2 ;;
esac
[ -d "$runtime_project" ] || { printf 'Runtime project is missing: %s\n' "$runtime_project" >&2; exit 1; }
[ -d "$runtime_data_dir" ] || { printf 'Runtime data directory is missing: %s\n' "$runtime_data_dir" >&2; exit 1; }

runtime_manifest="$runtime_project/bi2read-runtime.toml"
[ -f "$runtime_manifest" ] || runtime_manifest="$runtime_project/bimyscribe-runtime.toml"
[ -f "$runtime_manifest" ] || { printf 'Runtime manifest is missing: %s\n' "$runtime_manifest" >&2; exit 1; }
runtime_values=$(python3 - "$runtime_manifest" <<'PY'
import pathlib, sys, tomllib
manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(manifest.get("backend", ""))
print(manifest.get("contract_version", ""))
print(manifest.get("compose_file", ""))
PY
)
runtime_backend=$(printf '%s\n' "$runtime_values" | sed -n '1p')
runtime_contract=$(printf '%s\n' "$runtime_values" | sed -n '2p')
compose_file=$(printf '%s\n' "$runtime_values" | sed -n '3p')
[ "$runtime_backend" = "docker-compose" ] || {
    printf 'Refusing non-Docker Runtime backend: %s\n' "$runtime_backend" >&2
    exit 1
}
[ "$runtime_contract" = "2" ] || {
    printf 'Runtime contract must be 2, found %s.\n' "$runtime_contract" >&2
    exit 1
}
[ -n "$compose_file" ] && [ -f "$runtime_project/$compose_file" ] || {
    printf 'Docker Compose file is missing: %s/%s\n' "$runtime_project" "$compose_file" >&2
    exit 1
}

config_file="$HOME/Library/Application Support/bi2read/config.toml"
[ -f "$config_file" ] || {
    printf 'Existing bi2read state is required before Docker-local installation: %s\n' "$config_file" >&2
    printf 'Launch a regular build once to initialize state, then rerun this installer.\n' >&2
    exit 1
}

mkdir -p "$build_root"
stage=$(mktemp -d "$build_root/.docker-local-install.XXXXXX")
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

export CARGO_TARGET_DIR="$build_root/cargo-target"
"$cargo_bin" build --manifest-path "$repo_root/Cargo.toml" --release --locked --bin bi2read
binary="$CARGO_TARGET_DIR/release/bi2read"
[ -x "$binary" ] || { printf 'Release binary was not produced: %s\n' "$binary" >&2; exit 1; }
file "$binary" | rg -q 'Mach-O 64-bit executable arm64' || {
    printf 'bi2read release binary is not arm64.\n' >&2
    exit 1
}

app_version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "$repo_root/Cargo.toml" | head -n 1)
build_number=$(python3 - "$repo_root/packaging/release.toml" <<'PY'
import pathlib, sys, tomllib
print(tomllib.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["build_number"])
PY
)
app="$stage/bi2read.app"
contents="$app/Contents"
resources="$contents/Resources"
mkdir -p "$contents/MacOS" "$resources/licenses"
cp "$binary" "$contents/MacOS/bi2read"
cp "$repo_root/packaging/macos/assets/AppIcon.icns" "$resources/AppIcon.icns"
cp "$repo_root/LICENSE" "$resources/licenses/bi2read-MIT.txt"
chmod 755 "$contents/MacOS/bi2read"
sed -e "s/@APP_VERSION@/$app_version/g" -e "s/@BUILD_NUMBER@/$build_number/g" \
    "$repo_root/packaging/macos/Info.plist.in" > "$contents/Info.plist"
plutil -lint "$contents/Info.plist" >/dev/null

codesign --force --sign - --timestamp=none --options runtime "$contents/MacOS/bi2read"
codesign --force --sign - --timestamp=none --options runtime \
    --entitlements "$repo_root/packaging/macos/entitlements.plist" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
[ ! -e "$resources/runtime" ] && [ ! -e "$resources/bin/uv" ] || {
    printf 'Docker-local bundle unexpectedly contains native Runtime or uv.\n' >&2
    exit 1
}
[ "$("$contents/MacOS/bi2read" --version)" = "bi2read $app_version" ] || {
    printf 'Installed binary version check failed.\n' >&2
    exit 1
}

timestamp=$(date '+%Y%m%d-%H%M%S')
config_backup="$config_file.pre-docker-local-$timestamp"
python3 - "$config_file" "$config_backup" "$runtime_project" "$runtime_data_dir" <<'PY'
import json, os, pathlib, shutil, sys, tempfile, tomllib
path, backup = map(pathlib.Path, sys.argv[1:3])
runtime_project, runtime_data = sys.argv[3:5]
text = path.read_text(encoding="utf-8")
lines = text.splitlines(keepends=True)

def replace_or_insert(key, value):
    rendered = f'{key} = {json.dumps(value, ensure_ascii=False)}\n'
    for index, line in enumerate(lines):
        if line.startswith(f"{key} ="):
            lines[index] = rendered
            return
    insert_at = next((i + 1 for i, line in enumerate(lines) if line.startswith("output_dir =")), 0)
    lines.insert(insert_at, rendered)

replace_or_insert("runtime_project", runtime_project)
replace_or_insert("runtime_data_dir", runtime_data)
tomllib.loads("".join(lines))
shutil.copy2(path, backup)
fd, temporary = tempfile.mkstemp(prefix=".config.toml.", dir=path.parent)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.writelines(lines)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
finally:
    if os.path.exists(temporary): os.unlink(temporary)
PY

install_parent=$(dirname "$install_app")
install_name=$(basename "$install_app")
incoming="$install_parent/.$install_name.installing-$timestamp"
backup_app="${install_app%.app}-backup-$timestamp.app"
[ ! -e "$incoming" ] || { printf 'Staging install path already exists: %s\n' "$incoming" >&2; exit 1; }
[ ! -e "$backup_app" ] || { printf 'Backup App path already exists: %s\n' "$backup_app" >&2; exit 1; }
ditto "$app" "$incoming"
codesign --verify --deep --strict --verbose=2 "$incoming"
if [ -e "$install_app" ]; then
    mv "$install_app" "$backup_app"
else
    backup_app="(no installed App was present)"
fi
mv "$incoming" "$install_app"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$install_app"

codesign --verify --deep --strict --verbose=2 "$install_app"
installed_version=$("$install_app/Contents/MacOS/bi2read" --version)
installed_sha=$(shasum -a 256 "$install_app/Contents/MacOS/bi2read" | awk '{print $1}')
if [ "$launch" = true ]; then
    open "$install_app"
fi

printf '\nDocker-local bi2read installed.\n'
printf 'App: %s\nVersion: %s\nSHA-256: %s\n' "$install_app" "$installed_version" "$installed_sha"
printf 'Runtime project: %s\nRuntime data: %s\n' "$runtime_project" "$runtime_data_dir"
printf 'Previous App backup: %s\nConfig backup: %s\n' "$backup_app" "$config_backup"
printf 'This ad-hoc bundle is for this Mac only and is not a public release artifact.\n'
