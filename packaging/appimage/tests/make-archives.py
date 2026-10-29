#!/usr/bin/env python3
"""Writes the archives the end-to-end test installs through TarDrop's window.

Built with Python's tarfile/zipfile rather than TarDrop's own crates, so hostile members such as
"../escape.txt" can be written at all, and the archives do not share the code that reads them.

    make-archives.py DIRECTORY
"""

import gzip
import io
import shutil
import sys
import tarfile
import zipfile
from pathlib import Path

# A real, tiny, never-executed ELF: TarDrop checks the magic and only ever runs it on request.
ELF = Path(shutil.which("true") or "/bin/true").read_bytes()
DESKTOP = b"[Desktop Entry]\nType=Application\nName=Hello World\nExec=hello\nIcon=hello\nCategories=Utility;\nComment=Says hello\n"
# The smallest valid PNG (1x1, transparent), so an icon is found and copied.
PNG = bytes.fromhex("89504e470d0a1a0a0000000d4948445200000001000000010806000000"
                    "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082")
# A launcher-named script: TarDrop accepts it as a launcher, and when the user presses Start it
# records the environment TarDrop gave it, which must be the host's, not the AppImage's.
ENVDUMP = b'#!/bin/sh\nmkdir -p "$HOME/env"\nenv > "$HOME/env/launched-app.env"\n'


def add(archive, name, data=b"", mode=0o644, kind=tarfile.REGTYPE, link=""):
    info = tarfile.TarInfo(name)
    info.type, info.mode, info.mtime, info.linkname = kind, mode, 1_700_000_000, link
    info.size = len(data) if kind == tarfile.REGTYPE else 0
    archive.addfile(info, io.BytesIO(data) if kind == tarfile.REGTYPE else None)


class gzipped:
    """A tar.gz writer whose gzip header carries no timestamp, so equal content means equal bytes."""
    def __init__(self, path):
        self.file = open(path, "wb")
        self.gzip = gzip.GzipFile(filename="", mode="wb", fileobj=self.file, mtime=0)
        self.tar = tarfile.open(fileobj=self.gzip, mode="w")

    def __enter__(self):
        return self.tar

    def __exit__(self, *_):
        self.tar.close(); self.gzip.close(); self.file.close()


def main(directory):
    out = Path(directory)
    out.mkdir(parents=True, exist_ok=True)

    # Clean: an ELF launcher, its desktop entry and icon. Installs without any prompt.
    with gzipped(out / "Hello.tar.gz") as archive:
        add(archive, "Hello/", kind=tarfile.DIRTYPE, mode=0o755)
        add(archive, "Hello/hello", ELF, 0o755)
        add(archive, "Hello/hello.desktop", DESKTOP)
        add(archive, "Hello/hello.png", PNG)
        add(archive, "Hello/README", b"hello\n")

    with tarfile.open(out / "Envdump.tar.xz", "w:xz") as archive:
        add(archive, "Envdump/run-envdump.sh", ENVDUMP, 0o755)

    # Risky: install.sh plus a symlink out of the package. High risk; the user refuses.
    with tarfile.open(out / "Risky.tar", "w") as archive:
        add(archive, "Risky/risky", ELF, 0o755)
        add(archive, "Risky/install.sh", b"#!/bin/sh\ntouch \"$HOME/pwned\"\n", 0o755)
        add(archive, "Risky/passwd", kind=tarfile.SYMTYPE, link="../../../../etc/passwd")

    # Traversal: a member that would land beside ~/Applications. Blocked; even after the user
    # chooses to trust it, that member must be skipped while the rest installs.
    with tarfile.open(out / "Traversal.tar.bz2", "w:bz2") as archive:
        add(archive, "Traversal/traversal", ELF, 0o755)
        add(archive, "../escape.txt", b"escaped\n")
        add(archive, "Traversal/../../escape2.txt", b"escaped\n")

    # Two equally likely launchers: TarDrop must ask instead of guessing.
    with gzipped(out / "Pair.tgz") as archive:
        add(archive, "Pair/alpha", ELF, 0o755)
        add(archive, "Pair/beta", ELF, 0o755)

    with zipfile.ZipFile(out / "Zipped.zip", "w", zipfile.ZIP_DEFLATED) as archive:
        entry = zipfile.ZipInfo("Zipped/zipped", date_time=(2023, 11, 14, 22, 13, 20))
        entry.external_attr = (0o100755 << 16)
        archive.writestr(entry, ELF)

    # Not an archive TarDrop supports; refused before anything is queued.
    (out / "notes.txt").write_text("not an archive\n")
    for path in sorted(out.iterdir()):
        print(path.name)


if __name__ == "__main__":
    main(sys.argv[1])
