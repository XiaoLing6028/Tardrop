#!/usr/bin/env python3
"""Compares two drive.py reports: a native build's and the AppImage's, from the same scenario.

They must agree on every check, on every file TarDrop wrote (paths, modes, link targets, contents,
desktop entries, database records), and on TarDrop's own technical log. Dialog texts are listed
when they differ but do not fail the comparison: the two builds use different libadwaita
versions, whose dialogs may expose different internal labels.

    compare.py NATIVE_REPORT APPIMAGE_REPORT
"""

import json
import sys
from difflib import unified_diff

native, appimage = (json.loads(open(path).read()) for path in sys.argv[1:3])
failures = 0


def compare(what, left, right):
    global failures
    if left == right:
        print(f"same      {what}")
        return
    failures += 1
    print(f"DIFFERENT {what}")
    left_text, right_text = (json.dumps(value, indent=1, sort_keys=True, ensure_ascii=False).splitlines() for value in (left, right))
    for line in unified_diff(left_text, right_text, "native", "appimage", lineterm="", n=1):
        print("    " + line)


compare("check results", [(c["check"], c["passed"]) for c in native["checks"]], [(c["check"], c["passed"]) for c in appimage["checks"]])
compare("files TarDrop wrote", native["filesystem"], appimage["filesystem"])
compare("technical log", (native.get("log") or "").splitlines(), (appimage.get("log") or "").splitlines())
compare("host programs TarDrop called", native["host_tool_calls"] and sorted({name.split(".")[0] for name in native["host_tool_calls"]}),
        appimage["host_tool_calls"] and sorted({name.split(".")[0] for name in appimage["host_tool_calls"]}))
for left, right in zip(native["dialogs"], appimage["dialogs"]):
    if left["labels"] != right["labels"]:
        print(f"note      dialog about {left['about']!r} exposes different labels: native {left['labels']} / appimage {right['labels']}")
print(f"\n{'IDENTICAL' if failures == 0 else f'{failures} DIFFERENCE(S)'}: native build vs AppImage")
sys.exit(1 if failures else 0)
