#!/usr/bin/env python3
"""Drives a running TarDrop through its accessibility tree (AT-SPI), as a user would, and reports.

Run by session.sh next to TarDrop and mock-portal.py. Each step picks archives through the file
chooser portal or presses buttons in the window and its dialogs, then waits for the outcome on
disk. The report is JSON: every check with its result, the dialogs seen, TarDrop's own technical
log, and a snapshot of everything TarDrop wrote, so a native build and an AppImage can be diffed.

    drive.py ARCHIVE_DIR REPORT_JSON
"""

import hashlib
import json
import os
import re
import sys
import time
import traceback
from pathlib import Path

import warnings

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

warnings.simplefilter("ignore", DeprecationWarning)

HOME = Path(os.environ["HOME"])
ARCHIVES = Path(sys.argv[1])
REPORT = Path(sys.argv[2])
SELECTION = Path(os.environ["MOCK_PORTAL_SELECTION"])
APPLICATIONS = HOME / "Applications"
LAUNCHERS = HOME / ".local/share/applications"
ICONS = HOME / ".local/share/icons/TarDrop"
DATABASE = HOME / ".local/share/tardrop/installed-apps.json"
report = {"checks": [], "dialogs": []}


def check(name, passed, detail=""):
    report["checks"].append({"check": name, "passed": bool(passed), "detail": str(detail)})
    print(("PASS " if passed else "FAIL ") + name + (f" — {detail}" if detail and not passed else ""), flush=True)
    return passed


def wait(what, ready, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = ready()
        except GLibError:
            value = None
        if value:
            return value
        time.sleep(0.2)
    raise TimeoutError(f"timed out waiting for {what}")


GLibError = gi.repository.GLib.Error


def children(node):
    try:
        return [node.get_child_at_index(i) for i in range(node.get_child_count())]
    except GLibError:
        return []


def walk(node):
    stack = [node]
    while stack:
        current = stack.pop()
        if current is None:
            continue
        yield current
        stack.extend(reversed(children(current)))


def label(node):
    try:
        return node.get_name() or ""
    except GLibError:
        return ""


def window():
    desktop = Atspi.get_desktop(0)
    for application in children(desktop):
        for top in children(application):
            if label(top) == "TarDrop":
                return top
    return None


def find(root, predicate):
    return next((node for node in walk(root) if predicate(node)), None)


def role(node):
    try:
        return node.get_role_name()
    except GLibError:
        return ""


def describe(node):
    try:
        return node.get_description() or ""
    except GLibError:
        return ""


def button(root, name):
    return find(root, lambda node: role(node) in ("button", "push button", "toggle button") and (label(node) == name or describe(node) == name))


def press(node, what):
    if node is None:
        raise LookupError(f"no {what}")
    action = node.get_action_iface()
    if action is None or action.get_n_actions() == 0:
        raise LookupError(f"{what} has no action")
    action.do_action(0)


def dialog(text):
    """The open alert whose heading or body contains text. AdwAlertDialog lives inside the window."""
    def matching():
        top = window()
        if top is None:
            return None
        for node in walk(top):
            if role(node) in ("dialog", "alert") and any(text in label(part) for part in walk(node)):
                return node
        return None
    return matching()


def text_of(node):
    """The contents of a text widget, or "" for any other node."""
    try:
        return Atspi.Text.get_text(node, 0, -1) if node.get_text_iface() is not None and role(node) != "label" else ""
    except GLibError:
        return ""


def texts(node):
    return [normal(label(part)) for part in walk(node) if role(part) == "label" and label(part)]


def answer(text, response, timeout=30):
    found = wait(f"a dialog about {text!r}", lambda: dialog(text), timeout)
    labels = texts(found)
    report["dialogs"].append({"about": text, "labels": labels, "answered": response})
    target = find(found, lambda node: role(node) in ("button", "push button") and label(node) == response)
    if target is None:
        target = find(found, lambda node: label(node) == response)
    press(target, f"{response!r} in the dialog about {text!r}")
    wait(f"the dialog about {text!r} to close", lambda: dialog(text) is None, 15)
    return labels


def choose(*names):
    """Queues names in the mock portal and presses the window's Open Archive… button."""
    SELECTION.write_text("".join(str(ARCHIVES / name) + "\n" for name in names))
    press(wait("the Open Archive… button", lambda: button(window(), "Open Archive…")), "Open Archive…")
    wait("the portal to take the selection", lambda: not SELECTION.exists(), 15)


def idle():
    """The Install page's activity row reads Idle once the queue has drained."""
    top = window()
    return top is not None and find(top, lambda node: label(node) == "Idle") is not None and dialog("") is None


def staging_left():
    return sorted(p.name for p in APPLICATIONS.glob(".tardrop-*")) if APPLICATIONS.exists() else []


def go_to(page):
    row = wait(f"the {page!r} sidebar row", lambda: find(window(), lambda node: role(node) in ("list item", "list box row", "table row") and any(label(part) == page for part in walk(node))))
    sidebar = row.get_parent()
    selection = sidebar.get_selection_iface() if sidebar is not None else None
    if selection is not None:
        selection.select_child(row.get_index_in_parent())
    else:
        press(row, f"the {page!r} row")
    wait(f"the {page!r} page", lambda: find(window(), lambda node: role(node) == "label" and label(node) == page and node.get_state_set().contains(Atspi.StateType.SHOWING)), 10)


# Expander rows expose no accessible action, and their collapsed contents are not in the tree, so
# they are opened with a synthesized click, which AT-SPI can only produce on X11.
CAN_CLICK = bool(os.environ.get("DISPLAY")) and not os.environ.get("WAYLAND_DISPLAY")


def skip(steps):
    report.setdefault("skipped", []).append(steps)
    print(f"SKIP {steps} (pointer synthesis is X11-only)", flush=True)


def click_point(extents):
    """Where to click a row: near its left edge, away from the toasts that float over the middle
    of the window, and 10 pixels below its middle, which stays inside a 55-pixel row whether or
    not positions are reported about 22 pixels too high, as GTK 4.16 does."""
    return extents.x + 24, extents.y + extents.height // 2 + 10


def reveal(node):
    """Wheel-scrolls the page until node's click point is inside the window; GTK cannot scroll over
    AT-SPI. With no window manager the window sits at the screen origin, so window coordinates
    are screen coordinates."""
    frame = window().get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    for _ in range(60):
        extents = node.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
        _, y = click_point(extents)
        if 60 <= extents.y and y + 30 <= frame.height:
            return extents
        Atspi.generate_mouse_event(frame.width * 2 // 3, frame.height // 2, "b5c" if y + 30 > frame.height else "b4c")
        time.sleep(0.1)
    raise RuntimeError(f"could not scroll {label(node)!r} into view")


def expand(row):
    """Clicks a row, which expands an expander row."""
    x, y = click_point(reveal(row))
    Atspi.generate_mouse_event(x, y, "b1c")


KEY_SHIFT_TAB, KEY_HOME, KEY_RETURN = ("shift", 0xff09), 0xff50, 0xff0d


def keys(*sequence):
    """Types keys into the focused window. AT-SPI synthesizes them through XTest, so X11 only."""
    for key in sequence:
        time.sleep(0.3)
        if isinstance(key, tuple):
            Atspi.generate_keyboard_event(0xffe1, None, Atspi.KeySynthType.SYM | Atspi.KeySynthType.PRESS)
            Atspi.generate_keyboard_event(key[1], None, Atspi.KeySynthType.SYM)
            Atspi.generate_keyboard_event(0xffe1, None, Atspi.KeySynthType.SYM | Atspi.KeySynthType.RELEASE)
        else:
            Atspi.generate_keyboard_event(key, None, Atspi.KeySynthType.SYM)


def expander(title):
    return wait(f"the {title!r} row", lambda: find(window(), lambda node: label(node) == title and any(button(node, b) for b in ("Start this application now.",))))


def host_environment_kept(what, dump):
    """A program TarDrop started must see the session's own environment, never the AppImage's."""
    child = dict(line.split("=", 1) for line in dump.splitlines() if "=" in line)
    problems = [f"{key}={child.get(key)!r}, session has {os.environ.get(key)!r}"
                for key in ("XDG_DATA_DIRS", "GIO_MODULE_DIR", "GDK_PIXBUF_MODULE_FILE", "GDK_DISABLE", "LD_LIBRARY_PATH", "PATH")
                if child.get(key) != os.environ.get(key)]
    problems += [f"{key} leaked" for key in child if key.startswith("TARDROP_HOST") or key in ("APPIMAGE", "APPDIR", "ARGV0", "OWD")]
    check(f"{what} gets the host environment", not problems, "; ".join(problems))


def normal(text):
    """HOME, the archive folder, and random staging names differ per run; nothing else may."""
    return re.sub(r"\.tardrop-[A-Za-z0-9]+", ".tardrop-*", text.replace(str(ARCHIVES), "$ARCHIVES").replace(str(HOME), "$HOME"))


def tardrop_environment():
    """TarDrop's own environment, from /proc, as name → value."""
    for process in Path("/proc").iterdir():
        try:
            if (process / "comm").read_text().startswith("tardrop"):
                return dict(item.split("=", 1) for item in (process / "environ").read_text().split("\0") if "=" in item)
        except OSError:
            continue
    return {}


def snapshot():
    """Everything TarDrop owns on disk, normalised."""
    entries = {}
    for root in (APPLICATIONS, LAUNCHERS, ICONS, DATABASE.parent):
        if not root.exists():
            continue
        for path in sorted([root, *root.rglob("*")]):
            key = normal(str(path))
            if path.is_symlink():
                entries[key] = {"symlink": os.readlink(path)}
            elif path.is_dir():
                entries[key] = {"dir": oct(path.stat().st_mode & 0o7777)}
            elif path.suffix in (".desktop",):
                entries[key] = {"mode": oct(path.stat().st_mode & 0o7777), "text": normal(path.read_text())}
            elif path.suffix == ".json":
                data = json.loads(path.read_text())
                for record in data if isinstance(data, list) else []:
                    for volatile in ("install_date", "last_update_check"):
                        record.pop(volatile, None)
                entries[key] = {"json": json.loads(normal(json.dumps(data)))}
            else:
                entries[key] = {"mode": oct(path.stat().st_mode & 0o7777), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
    return entries


def run():
    top = wait("TarDrop's window", window, 60)
    check("window is exposed with title TarDrop", top is not None)
    for page in ("Install", "Installed Applications", "Updates", "Settings"):
        check(f"sidebar offers {page}", find(top, lambda node: label(node) == page) is not None)
    check("drop surface invites a drop", find(top, lambda node: label(node) == "Drop an Archive Here") is not None)

    # A clean archive installs without any question.
    choose("Hello.tar.gz")
    hello = APPLICATIONS / "Hello World"
    wait("Hello World to be installed", lambda: (hello / "hello").exists() and idle())
    check("clean tar.gz installs under ~/Applications", (hello / "hello").is_file() and os.access(hello / "hello", os.X_OK))
    entry = LAUNCHERS / "org.tardrop.Hello-World.desktop"
    check("launcher written with the package's Name", entry.is_file() and "Name=Hello World\n" in entry.read_text())
    check("launcher Exec points into ~/Applications", entry.is_file() and f'Exec="{hello}/hello"' in entry.read_text(), entry.read_text() if entry.exists() else "")
    check("icon copied into XDG data", (ICONS / "org.tardrop.Hello-World.png").is_file())
    check("install recorded in the database", DATABASE.is_file() and "Hello World" in DATABASE.read_text())

    # The environment TarDrop gives the programs it starts.
    choose("Envdump.tar.xz")
    envdump = APPLICATIONS / "Run Envdump"
    wait("Run Envdump to be installed", lambda: (envdump / "run-envdump.sh").exists() and idle())
    check("tar.xz with a named launcher script installs", (envdump / "run-envdump.sh").is_file())
    row = expander("Run Envdump")
    press(button(row, "Start this application now."), "Start for Run Envdump")
    launched = HOME / "env/launched-app.env"
    wait("the started application to record its environment", launched.exists, 15)
    report["launched_env"] = launched.read_text()
    # The control: what TarDrop itself runs with. In the AppImage this differs from the session
    # (AppRun's changes), which is what makes the clean environments above meaningful.
    own = tardrop_environment()
    report["tardrop_env_changes"] = {key: own.get(key) for key in sorted(set(own) | set(os.environ)) if own.get(key) != os.environ.get(key)}
    print("INFO TarDrop's own environment differs from the session in: " + ", ".join(report["tardrop_env_changes"]), flush=True)
    host_environment_kept("an installed application started from TarDrop", report["launched_env"])
    if CAN_CLICK:
        expand(row)
        press(wait("Open Folder", lambda: button(window(), "Open Folder")), "Open Folder")
        opened = wait("xdg-open to be called", lambda: sorted((HOME / "env").glob("xdg-open.*.env")), 15)
        check("Open Folder hands xdg-open the install folder", opened[0].with_suffix(".args").read_text() == f"{envdump}\n")
        host_environment_kept("xdg-open", opened[0].read_text())
    else:
        skip("Open Folder through xdg-open")
    refreshed = sorted((HOME / "env").glob("update-desktop-database.*.env"))
    check("installs refresh the desktop database", bool(refreshed))
    for tool in ("update-desktop-database", "gtk-update-icon-cache", "kbuildsycoca6"):
        calls = sorted((HOME / "env").glob(f"{tool}.*.env"))
        if calls:
            host_environment_kept(tool, calls[0].read_text())

    # High risk: install.sh plus an escaping symlink. Refusing must leave nothing behind.
    choose("Risky.tar")
    risk = answer("Review Installation Risk", "Don’t Install")
    level = [text for text in risk if text.startswith("High — ")]
    check("risk prompt shows High with its reasons", level == ["High — install.sh script found (1), 1 symlink escapes the package"], level)
    wait("the queue to settle", idle)
    check("refused archive leaves nothing in ~/Applications", not (APPLICATIONS / "Risky").exists() and not staging_left(), staging_left())
    check("install.sh was never run", not (HOME / "pwned").exists())

    # Path traversal: blocked; trusting it installs the package but never the escaping members.
    choose("Traversal.tar.bz2")
    blocked = answer("Installation Blocked", "Trust and Install")
    check("traversal is blocked with its reason", any("unsafe path: ../escape.txt" in text for text in blocked), blocked)
    wait("Traversal to be installed", lambda: (APPLICATIONS / "Traversal/traversal").exists() and idle())
    escaped = [p for p in (APPLICATIONS / "escape.txt", APPLICATIONS / "escape2.txt", HOME / "escape.txt", HOME / "escape2.txt") if p.exists()]
    check("trusted archive still never writes outside its folder", not escaped, escaped)

    # Equally likely launchers: TarDrop asks. Its rows, like expander rows, need a click.
    if CAN_CLICK:
        choose("Pair.tgz")
        chooser = wait("the launcher chooser", lambda: dialog("Choose Application Launcher"))
        report["dialogs"].append({"about": "Choose Application Launcher", "labels": texts(chooser), "answered": "alpha"})
        check("launcher chooser lists both candidates", {"alpha", "beta"} <= set(texts(chooser)), texts(chooser))
        # GTK 4.16 reports positions inside dialogs without their scrolled offsets, so a click
        # would miss; the keyboard answers instead. Focus starts on Cancel Installation:
        # Shift+Tab enters the list, Home picks its first row, alpha, and Return activates it.
        focused = find(chooser, lambda node: node.get_state_set().contains(Atspi.StateType.FOCUSED))
        check("launcher chooser focuses Cancel Installation first", label(focused) == "Cancel Installation", label(focused))
        keys(KEY_SHIFT_TAB, KEY_HOME, KEY_RETURN)
        wait("Alpha to be installed", lambda: (APPLICATIONS / "Alpha/alpha").exists() and idle())
        check("launcher chooser installs the chosen launcher", "Alpha/alpha" in (LAUNCHERS / "org.tardrop.Alpha.desktop").read_text())
    else:
        skip("choosing between equally likely launchers")

    # ZIP, then the same ZIP again: TarDrop asks before touching the existing install.
    choose("Zipped.zip")
    wait("Zipped to be installed", lambda: (APPLICATIONS / "Zipped/zipped").exists() and idle())
    check("zip installs with its executable bit", os.access(APPLICATIONS / "Zipped/zipped", os.X_OK))
    choose("Zipped.zip")
    answer("Application Already Installed", "Replace")
    wait("Zipped to be replaced", idle)
    check("replace keeps a single Zipped", sorted(p.name for p in APPLICATIONS.iterdir() if p.name.startswith("Zipped")) == ["Zipped"])

    # Unsupported files are refused before anything is queued.
    choose("notes.txt")
    refused = answer("TarDrop Ran Into a Problem", "Close")
    check("unsupported file is refused", any("Unsupported archive type" in text for text in refused), refused)

    # Durable records: an update check with no configured source fails cleanly, offline; then
    # uninstalling removes exactly what TarDrop made.
    go_to("Installed Applications")
    if CAN_CLICK:
        record = lambda: find(window(), lambda node: role(node) == "list item" and label(node) == "Hello World" and node.get_state_set().contains(Atspi.StateType.SHOWING))
        expand(wait("the Hello World record", record))
        press(wait("Check for Updates", lambda: button(window(), "Check for Updates")), "Check for Updates")
        failed = answer("TarDrop Ran Into a Problem", "Close")
        check("update check without a source reports, not crashes", any("Update check failed" in text for text in failed), failed)
        # A finished check rebuilds the rows, which collapses them again.
        expand(wait("the Hello World record", record))
        press(wait("Uninstall", lambda: button(window(), "Uninstall")), "Uninstall for Hello World")
        answer("Uninstall Hello World?", "Uninstall")
        wait("Hello World to be removed", lambda: not hello.exists())
        check("uninstall removes the folder, launcher, and icon", not hello.exists() and not entry.exists() and not (ICONS / "org.tardrop.Hello-World.png").exists())
        check("uninstall removes the record", "Hello World" not in DATABASE.read_text())
    else:
        skip("update check and uninstall from the Installed Applications page")

    go_to("Install")
    if CAN_CLICK:
        expand(wait("the technical log row", lambda: find(window(), lambda node: role(node) == "list item" and label(node) == "Technical installation log")))
        log_view = wait("the technical log", lambda: find(window(), lambda node: "Ready." in text_of(node)))
        report["log"] = normal(text_of(log_view))
        check("technical log records the refusals and the skipped member", all(line in report["log"] for line in (
            "Installation blocked by safety checks.", "Skipped member outside the install folder: ../escape.txt")), report["log"])
    else:
        skip("reading the technical log")
    check("no staging directory is ever left behind", not staging_left(), staging_left())


def dump_tree(path):
    """The window's accessibility tree with each node's actions, for diagnosing a failed step."""
    lines = []
    def visit(node, depth):
        action = node.get_action_iface()
        actions = [Atspi.Action.get_action_name(action, i) for i in range(action.get_n_actions())] if action else []
        lines.append(f"{'  ' * depth}{role(node)} {label(node)!r} {describe(node)!r} {actions}")
        for child in children(node):
            visit(child, depth + 1)
    top = window()
    if top is not None:
        visit(top, 0)
    path.write_text("\n".join(lines))


try:
    run()
except Exception as error:  # the report must be written even when a step fails
    check("scenario completed", False, f"{error}\n{traceback.format_exc()}")
    dump_tree(REPORT.with_name("tree.txt"))
else:
    check("scenario completed", True)
report["filesystem"] = snapshot()
report["host_tool_calls"] = sorted(p.name for p in (HOME / "env").glob("*.env")) if (HOME / "env").exists() else []
report["host_tool_env"] = {p.name: p.read_text() for p in sorted((HOME / "env").glob("*.env"))} if (HOME / "env").exists() else {}
REPORT.write_text(json.dumps(report, indent=2, ensure_ascii=False))
sys.exit(0 if all(item["passed"] for item in report["checks"]) else 1)
