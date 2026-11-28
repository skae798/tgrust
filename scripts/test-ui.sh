#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf '%s\n' \
        'Usage: scripts/test-ui.sh [--wayland] [FILTER]' \
        '       scripts/test-ui.sh [--wayland] --demo [--binary PATH]' \
        'Runs GTK tests or a bounded demo on a private display and D-Bus session.' \
        'Wayland requires Mutter; the default display requires Xvfb.'
}

backend=x11
mode=tests
binary=
filters=()
while (( $# )); do
    case "$1" in
        --wayland) backend=wayland ;;
        --demo) mode=demo ;;
        --binary)
            if (( $# < 2 )); then usage >&2; exit 2; fi
            binary=$2
            [[ $binary == /* ]] || binary="$PWD/$binary"
            shift
            ;;
        --help|-h) usage; exit 0 ;;
        -*) usage >&2; exit 2 ;;
        *) filters+=("$1") ;;
    esac
    shift
done
if [[ -n $binary && $mode != demo ]] || (( ${#filters[@]} > 1 )) ||
    [[ $mode == demo && ${#filters[@]} != 0 ]]; then
    usage >&2
    exit 2
fi
for tool in dbus-run-session dbus-update-activation-environment timeout; do
    command -v "$tool" >/dev/null || { printf 'Required: %s\n' "$tool" >&2; exit 1; }
done
if [[ $backend == wayland ]]; then
    command -v mutter >/dev/null || { printf '%s\n' 'Required: mutter' >&2; exit 1; }
else
    command -v xvfb-run >/dev/null || { printf '%s\n' 'Required: xvfb-run' >&2; exit 1; }
fi

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
runtime=$(mktemp -d /tmp/tgrust-ui.XXXXXXXX)
cleanup() {
    local result=$?
    if (( result == 0 )); then
        rm -rf -- "$runtime"
    else
        printf 'UI check failed; isolated logs: %s\n' "$runtime" >&2
    fi
}
trap cleanup EXIT
mkdir -- "$runtime/data" "$runtime/config" "$runtime/cache"
# These changes apply only to the child session, never to GNOME or the launcher.
env -u DISPLAY -u XAUTHORITY -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u GDK_BACKEND \
    -u DBUS_SESSION_BUS_ADDRESS -u DBUS_SESSION_BUS_PID -u DBUS_STARTER_ADDRESS \
    -u DBUS_STARTER_BUS_TYPE -u AT_SPI_BUS_ADDRESS \
    XDG_RUNTIME_DIR="$runtime" XDG_DATA_HOME="$runtime/data" \
    XDG_CONFIG_HOME="$runtime/config" XDG_CACHE_HOME="$runtime/cache" \
    GTK_A11Y=none GSETTINGS_BACKEND=memory GIO_USE_VFS=local \
    dbus-run-session -- bash -s -- "$backend" "$mode" "$binary" "${filters[@]}" <<'SESSION'
set -euo pipefail
backend=$1
mode=$2
binary=$3
shift 3
if [[ $backend == wayland ]]; then
    # No X11 fallback and no forced GTK backend: verify normal Wayland selection.
    export WAYLAND_DISPLAY=tgrust-test
    mutter --headless --no-x11 --wayland --wayland-display="$WAYLAND_DISPLAY" \
        --virtual-monitor=1024x768 >"$XDG_RUNTIME_DIR/compositor.log" 2>&1 &
    compositor=$!
    trap 'kill "$compositor" 2>/dev/null || true; wait "$compositor" 2>/dev/null || true' EXIT
    for ((attempt=0; attempt<100; attempt++)); do
        [[ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] || break
        if ! kill -0 "$compositor" 2>/dev/null; then
            cat "$XDG_RUNTIME_DIR/compositor.log" >&2
            exit 1
        fi
        sleep 0.1
    done
    if [[ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]; then
        cat "$XDG_RUNTIME_DIR/compositor.log" >&2
        exit 1
    fi
    # This is the private bus. Never pass --systemd: GNOME must stay untouched.
    dbus-update-activation-environment WAYLAND_DISPLAY
    display_command=()
else
    export GDK_BACKEND=x11
    display_command=(xvfb-run -a -s '-screen 0 1920x1440x24' bash -c '
        set -e
        dbus-update-activation-environment DISPLAY XAUTHORITY GDK_BACKEND
        exec "$@"
    ' --)
fi
if [[ $mode == tests ]]; then
    "${display_command[@]}" cargo test --locked "$@" -- --ignored --nocapture --test-threads=1
else
    if [[ -z $binary ]]; then
        # Compile before the bounded run, so a slow build cannot count as a demo.
        cargo build --locked
        demo_command=(cargo run --locked -- --demo)
    else
        demo_command=("$binary" --demo)
    fi
    if [[ $backend == wayland ]]; then export WAYLAND_DEBUG=client; fi
    set +e
    "${display_command[@]}" timeout --signal=INT --kill-after=3s 8s "${demo_command[@]}" \
        >"$XDG_RUNTIME_DIR/demo.log" 2>&1
    result=$?
    set -e
    if [[ $result != 0 && $result != 124 ]]; then
        tail -80 "$XDG_RUNTIME_DIR/demo.log" >&2
        exit 1
    fi
    if [[ $backend == wayland ]]; then
        # A live surface proves the executable opened a Wayland window.
        if ! grep -q 'wl_surface.*commit' "$XDG_RUNTIME_DIR/demo.log"; then
            tail -80 "$XDG_RUNTIME_DIR/demo.log" >&2
            exit 1
        fi
        printf '%s\n' 'Demo displayed a surface through Wayland with GDK_BACKEND unset.'
    else
        printf '%s\n' 'Demo completed its bounded run on isolated Xvfb.'
    fi
fi
SESSION
