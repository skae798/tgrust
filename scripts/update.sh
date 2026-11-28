#!/usr/bin/env bash
set -euo pipefail

readonly repo_url=https://github.com/skae798/tgrust.git
readonly app_id=io.github.skae798.tgrust
readonly cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/tgrust-update"
readonly data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
readonly bin_dir="$HOME/.local/bin"

if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    printf '%s\n' 'Использование: update.sh' 'Собирает tgrust из ветки main на GitHub и устанавливает его для текущего пользователя.'
    exit 0
fi
if (( $# != 0 )); then
    printf '%s\n' 'Неизвестные аргументы. Используйте --help.' >&2
    exit 2
fi

for tool in git cargo cmake c++ flock tar desktop-file-edit desktop-file-validate; do
    if ! command -v "$tool" >/dev/null; then
        printf 'Требуется %s. См. зависимости сборки в README.md.\n' "$tool" >&2
        exit 1
    fi
done

mkdir -p -- "$cache_dir"
exec 9>"$cache_dir/update.lock"
if ! flock -n 9; then
    printf '%s\n' 'Обновление tgrust уже выполняется.' >&2
    exit 1
fi

git_dir="$cache_dir/repo"
git init --quiet --bare -- "$git_dir"
git_options=(--git-dir="$git_dir")
if command -v gh >/dev/null; then
    git_options+=(-c 'credential.https://github.com.helper=!gh auth git-credential')
fi

if ! GIT_TERMINAL_PROMPT=0 git "${git_options[@]}" fetch --quiet --depth=1 "$repo_url" refs/heads/main; then
    printf '%s\n' 'Не удалось загрузить приватный репозиторий. Проверьте доступ через gh auth login или настройки Git.' >&2
    exit 1
fi
revision=$(git "${git_options[@]}" rev-parse 'FETCH_HEAD^{commit}')

source_dir=$(mktemp -d "$cache_dir/source.XXXXXXXX")
staged_files=()
cleanup() {
    if (( ${#staged_files[@]} )); then
        rm -- "${staged_files[@]}" 2>/dev/null || true
    fi
    rm -r -- "$source_dir" || true
}
trap cleanup EXIT

git "${git_options[@]}" archive "$revision" | tar -x -C "$source_dir"
printf 'Сборка tgrust из main (%s)…\n' "${revision:0:12}"
cargo build --locked --release --manifest-path "$source_dir/Cargo.toml" --target-dir "$cache_dir/target"
"$cache_dir/target/release/tgrust" --help >/dev/null

icon_rel="icons/hicolor/256x256/apps/$app_id.png"
icon="$data_dir/$icon_rel"
desktop="$data_dir/applications/$app_id.desktop"
binary="$bin_dir/tgrust"

# desktop-file-edit экранирует значение для файла. Кавычки и знаки процента
# внутри Exec обрабатываются по правилам запуска Desktop Entry.
exec_path=${binary//\\/\\\\}
exec_path=${exec_path//\"/\\\"}
exec_path=${exec_path//\$/\\\$}
exec_path=${exec_path//\`/\\\`}
exec_path=${exec_path//%/%%}
# GIO проверяет первое слово Exec до раскрытия кодов вроде %%.
desktop-file-edit --set-key=Exec --set-value="/usr/bin/env -- \"$exec_path\"" \
    --set-icon="$icon" "$source_dir/data/$app_id.desktop"
desktop-file-validate "$source_dir/data/$app_id.desktop"

stage_file() {
    local source=$1 destination=$2 mode=$3 temporary
    mkdir -p -- "$(dirname -- "$destination")"
    temporary=$(mktemp "${destination}.XXXXXXXX")
    staged_files+=("$temporary")
    install -m "$mode" -- "$source" "$temporary"
}
stage_file "$source_dir/data/$icon_rel" "$icon" 0644
stage_file "$source_dir/data/$app_id.desktop" "$desktop" 0644
stage_file "$cache_dir/target/release/tgrust" "$binary" 0755

mv -fT -- "${staged_files[0]}" "$icon"
mv -fT -- "${staged_files[1]}" "$desktop"
mv -fT -- "${staged_files[2]}" "$binary"
staged_files=()
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$data_dir/applications" || true
fi

printf 'Установлен tgrust из коммита %s.\n' "${revision:0:12}"
printf '%s\n' 'Если клиент открыт, перезапустите его, чтобы запустить новую сборку.'
