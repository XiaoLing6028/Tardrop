#!/bin/bash
# Runs one TarDrop build in a throwaway desktop session and drives it with drive.py.
#
#   session.sh x11|wayland REPORT_DIR COMMAND...
#
# COMMAND starts TarDrop: the AppImage, or a native build to compare against. The session has its
# own display (Xvfb, or a headless Weston for Wayland), D-Bus session bus, accessibility bus, a
# mock file chooser portal, and an empty HOME. update-desktop-database, gtk-update-icon-cache,
# kbuildsycoca6, and xdg-open are replaced by stubs that record their arguments and environment,
# so the report shows exactly what TarDrop hands the host programs it starts.
set -euo pipefail

backend=$1 reports=$2
shift 2
tests=$(dirname "$(readlink -f "$0")")
mkdir -p "$reports"

HOME=$(mktemp -d /tmp/tardrop-home.XXXXXX)
export HOME
unset XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME GIO_MODULE_DIR GDK_PIXBUF_MODULE_FILE GDK_DISABLE APPIMAGE APPDIR ARGV0 OWD
# A distinctive host value that every program TarDrop starts must receive unchanged.
export XDG_DATA_DIRS=/usr/local/share:/usr/share:/opt/tardrop-test-marker/share
XDG_RUNTIME_DIR=$(mktemp -d /tmp/tardrop-runtime.XXXXXX)
export XDG_RUNTIME_DIR
export NO_AT_BRIDGE=0 GTK_A11Y=atspi
export MOCK_PORTAL_SELECTION=$HOME/.portal-selection

mkdir -p "$HOME/.stubs" "$HOME/env"
for tool in xdg-open update-desktop-database gtk-update-icon-cache kbuildsycoca6; do
    cat > "$HOME/.stubs/$tool" << EOF
#!/bin/sh
call=\$(date +%s%N)
env > "\$HOME/env/$tool.\$call.env"
printf '%s\n' "\$@" > "\$HOME/env/$tool.\$call.args"
EOF
    chmod +x "$HOME/.stubs/$tool"
done
export PATH=$HOME/.stubs:$PATH

archives=$(mktemp -d /tmp/tardrop-archives.XXXXXX)
python3 -I "$tests/make-archives.py" "$archives" > /dev/null

cleanup() { jobs -p | xargs -r kill 2> /dev/null || true; }
trap cleanup EXIT

case $backend in
    x11)
        Xvfb :87 -screen 0 1280x800x24 -nolisten tcp > "$reports/display.log" 2>&1 &
        for _ in $(seq 100); do [[ -S /tmp/.X11-unix/X87 ]] && break; sleep 0.1; done
        export DISPLAY=:87
        unset WAYLAND_DISPLAY
        ;;
    wayland)
        weston --backend=headless --socket=tardrop-wayland --idle-time=0 > "$reports/display.log" 2>&1 &
        for _ in $(seq 100); do [[ -S $XDG_RUNTIME_DIR/tardrop-wayland ]] && break; sleep 0.1; done
        export WAYLAND_DISPLAY=tardrop-wayland
        unset DISPLAY
        ;;
    *) echo "unknown backend $backend" >&2; exit 2 ;;
esac

# The session bus, the accessibility bus it activates, the portal, TarDrop, and the driver.
export TESTS=$tests ARCHIVES=$archives REPORTS=$reports BACKEND=$backend
dbus-run-session -- bash -c '
    python3 -I "$TESTS/mock-portal.py" > "$REPORTS/portal.log" 2>&1 &
    for _ in $(seq 100); do grep -q "mock portal ready" "$REPORTS/portal.log" && break; sleep 0.1; done
    "$@" > "$REPORTS/tardrop.log" 2>&1 &
    tardrop=$!
    status=0
    python3 -I "$TESTS/drive.py" "$ARCHIVES" "$REPORTS/report.json" > "$REPORTS/drive.log" 2>&1 || status=$?
    if [[ $BACKEND == x11 ]] && command -v import > /dev/null; then import -window root "$REPORTS/screenshot.png" 2> /dev/null || true; fi
    kill -0 $tardrop 2> /dev/null && echo "TarDrop still running at the end: yes" >> "$REPORTS/drive.log"
    # A missing loader or MIME database shows up only as GTK warnings, never as a failed step.
    if grep -q "Failed to load icon" "$REPORTS/tardrop.log"; then echo "FAIL icons failed to load (see tardrop.log)" >> "$REPORTS/drive.log"; status=1; fi
    kill $tardrop 2> /dev/null || true
    wait $tardrop 2> /dev/null || true
    exit $status
' tardrop-session "$@"
