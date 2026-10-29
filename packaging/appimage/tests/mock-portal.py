#!/usr/bin/env python3
"""A stand-in for xdg-desktop-portal's file chooser, for the end-to-end test.

TarDrop's "Open Archive…" uses GtkFileDialog, which asks org.freedesktop.portal.FileChooser over
D-Bus exactly as it does under GNOME or KDE. This service answers OpenFile with the paths listed
in the file named by $MOCK_PORTAL_SELECTION (one per line, consumed on use), so the test drives
the real file-selection path without a visible dialog. Each request is logged to stdout.
"""

import os
from pathlib import Path

import gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

INTERFACE = """
<node>
  <interface name="org.freedesktop.portal.FileChooser">
    <method name="OpenFile">
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="o" name="handle" direction="out"/>
    </method>
    <property name="version" type="u" access="read"/>
  </interface>
</node>
"""
SELECTION = Path(os.environ["MOCK_PORTAL_SELECTION"])


def call(connection, sender, path, interface, method, parameters, invocation):
    _parent, title, options = parameters.unpack()
    token = options.get("handle_token", "t")
    handle = f"/org/freedesktop/portal/desktop/request/{sender[1:].replace('.', '_')}/{token}"
    invocation.return_value(GLib.Variant("(o)", (handle,)))
    paths = [line for line in SELECTION.read_text().splitlines() if line] if SELECTION.exists() else []
    SELECTION.unlink(missing_ok=True)
    print(f"OpenFile title={title!r} multiple={options.get('multiple')} -> {paths}", flush=True)

    def respond():
        # Response 0 is "the user chose", 1 is "cancelled": an empty selection behaves like Cancel.
        results = {"uris": GLib.Variant("as", [Gio.File.new_for_path(p).get_uri() for p in paths])}
        connection.emit_signal(sender, handle, "org.freedesktop.portal.Request", "Response",
                               GLib.Variant("(ua{sv})", (0 if paths else 1, results)))
        return GLib.SOURCE_REMOVE
    # GTK subscribes to the Response signal before calling, so answering promptly is safe.
    GLib.timeout_add(150, respond)


def get_property(connection, sender, path, interface, name):
    return GLib.Variant("u", 4)


def acquired(connection, name):
    node = Gio.DBusNodeInfo.new_for_xml(INTERFACE)
    connection.register_object("/org/freedesktop/portal/desktop", node.interfaces[0], call, get_property, None)


def owned(connection, name):
    print("mock portal ready", flush=True)


Gio.bus_own_name(Gio.BusType.SESSION, "org.freedesktop.portal.Desktop", Gio.BusNameOwnerFlags.NONE, acquired, owned, lambda *_: os._exit(1))
GLib.MainLoop().run()
