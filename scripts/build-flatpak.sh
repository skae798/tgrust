#!/usr/bin/env bash
set -euo pipefail

usage='Использование: build-flatpak.sh [путь к пакету.flatpak]'
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    printf '%s\n' "$usage"
    exit 0
fi
if (( $# > 1 )); then
    printf '%s\n' "$usage" >&2
    exit 2
fi

app_id=io.github.skae798.tgrust
root=$(git -C "$(dirname -- "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)
revision=$(git -C "$root" rev-parse HEAD)
bundle=$(realpath -m -- "${1:-tgrust.flatpak}")
cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/tgrust-flatpak"

mkdir -p -- "$cache_dir"
exec 9>"$cache_dir/build.lock"
flock 9

stage=$(mktemp -d "$cache_dir/source.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
mkdir -- "$stage/source"
git -C "$root" archive "$revision" Cargo.toml Cargo.lock LICENSE data src patches | tar -x -C "$stage/source"
git -C "$root" show "$revision:packaging/flatpak/$app_id.yml" > "$stage/$app_id.yml"
(
    cd -- "$stage/source"
    mkdir -p .cargo
    cargo vendor --locked vendor > .cargo/config.toml
)

flatpak-builder --user --disable-rofiles-fuse --force-clean \
    --state-dir="$cache_dir/builder" --repo="$cache_dir/repo" \
    "$cache_dir/build" "$stage/$app_id.yml"
flatpak build-bundle "$cache_dir/repo" "$bundle" "$app_id" \
    --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo
printf 'Пакет из коммита %s: %s\n' "${revision:0:12}" "$bundle"
