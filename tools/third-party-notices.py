#!/usr/bin/env python3
"""Write the third-party licence notices of the `spaces` and `refs` binaries.

Every crate the two binaries link (the normal dependencies of spaces-cli and
refs-cli for TARGET, not those of builds or tests) with its licence and the
licence files it ships; the Rust standard library, which every Rust binary
links, from the toolchain's own notices; and musl for the static builds
(TARGET ending in -musl). The packages ship the file (the deb in
/usr/share/doc/storage-spaces, the Arch package in /usr/share/licenses).

Usage: tools/third-party-notices.py [--target TRIPLE] [--list] OUTPUT
       (--list prints the crates and their licences instead)
Run from the repository after the crates are fetched (cargo fetch).
"""

import argparse
import html
import json
import os
import re
import subprocess
import sys

LICENCE_FILE = re.compile(r"^(licen[cs]e|copying|notice|copyright|unlicense)([.-].*)?$", re.I)

MUSL = """musl libc, statically linked into the x86_64-unknown-linux-musl builds
(the copy the Rust toolchain ships for that target). Licence: MIT.

Copyright (c) 2005-2020 Rich Felker, et al.

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

Portions of musl (public headers, crt files and the dynamic linker
entry) are in the public domain or under other permissive terms; see
the COPYRIGHT file of musl (https://musl.libc.org/).
"""


def linked_crates(metadata):
    """The packages the binaries link: normal dependencies of the CLIs."""
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    workspace = set(metadata["workspace_members"])
    todo = [i for i in workspace if packages[i]["name"] in ("spaces-cli", "refs-cli")]
    seen = set()
    while todo:
        i = todo.pop()
        if i in seen:
            continue
        seen.add(i)
        # A procedural macro runs when building; neither it nor what it
        # needs is linked.
        if any("proc-macro" in t["kind"] for t in packages[i]["targets"]):
            continue
        for dep in nodes[i]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    linked = (
        packages[i]
        for i in seen - workspace
        if not any("proc-macro" in t["kind"] for t in packages[i]["targets"])
    )
    return sorted(linked, key=lambda p: (p["name"], p["version"]))


def licence_files(package):
    directory = os.path.dirname(package["manifest_path"])
    names = sorted(f for f in os.listdir(directory) if LICENCE_FILE.match(f))
    out = []
    for name in names:
        path = os.path.join(directory, name)
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as f:
                out.append((name, f.read().strip("\n")))
    return out


def toolchain_notices():
    """The Rust standard library's notices from the toolchain in use (a
    rustup toolchain keeps them in share/doc/rust, distributions' packages
    in share/doc/rust or share/licenses/rust)."""
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True).stdout.strip()
    version = subprocess.run(["rustc", "--version"], capture_output=True, text=True, check=True).stdout.strip()
    dirs = [os.path.join(base, *sub) for base in (sysroot, "/usr") for sub in (("share", "doc", "rust"), ("share", "licenses", "rust"))]
    parts = [f"The Rust standard library ({version}), linked into every Rust binary."]
    for d in dirs:
        for name in ("COPYRIGHT-library.html", "COPYRIGHT.html", "COPYRIGHT-library", "COPYRIGHT"):
            path = os.path.join(d, name)
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as f:
                    text = f.read()
                if name.endswith(".html"):
                    text = html.unescape(re.sub(r"<[^>]+>", "", text))
                text = "\n".join(line.rstrip() for line in text.splitlines())
                parts.append(re.sub(r"\n{3,}", "\n\n", text).strip())
                break
        else:
            continue
        break
    texts = {}
    for d in dirs:
        for name in sorted(os.listdir(os.path.join(d, "licenses"))) if os.path.isdir(os.path.join(d, "licenses")) else []:
            texts.setdefault(name.rsplit(".", 1)[0], os.path.join(d, "licenses", name))
        for name in ("LICENSE-MIT", "LICENSE-APACHE"):
            if os.path.isfile(os.path.join(d, name)):
                texts.setdefault({"LICENSE-MIT": "MIT", "LICENSE-APACHE": "Apache-2.0"}[name], os.path.join(d, name))
    for name in ("MIT", "Apache-2.0", "LLVM-exception", "NCSA", "BSD-2-Clause", "ISC"):
        if name in texts:
            with open(texts[name], encoding="utf-8") as f:
                parts.append(f"--- {name} ---\n\n" + f.read().strip("\n"))
    if len(parts) < 3:
        sys.exit(f"no Rust licence notices in {', '.join(dirs)}: install the toolchain's docs (rustup component add rust-docs)")
    return "\n\n".join(parts)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", default="x86_64-unknown-linux-musl")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("output", nargs="?")
    args = parser.parse_args()
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", args.target],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    crates = linked_crates(metadata)
    if args.list:
        for p in crates:
            files = [n for n, _ in licence_files(p)]
            print(f"{p['name']} {p['version']}: {p['license']} {files}")
        return
    if not args.output:
        parser.error("OUTPUT is needed without --list")
    sections = [
        "Third-party software in spaces and refs (Storage Spaces and ReFS for Linux).\n\n"
        "These binaries are licensed under GPL-2.0-or-later (see the copyright file). They\n"
        "link the following software, under the licences given; each section holds the\n"
        "licence files the software ships."
    ]
    for p in crates:
        head = f"{p['name']} {p['version']}\nLicence: {p['license'] or 'see the files below'}"
        if p.get("repository"):
            head += f"\nSource: {p['repository']}"
        files = licence_files(p)
        if not files:
            head += "\n(The crate ships no licence file; its licence is the SPDX expression above.)"
        body = "\n\n".join(f"--- {name} ---\n\n{text}" for name, text in files)
        sections.append(head + ("\n\n" + body if body else ""))
    sections.append(toolchain_notices())
    if args.target.endswith("-musl"):
        sections.append(MUSL.strip("\n"))
    rule = "\n\n" + "=" * 78 + "\n\n"
    with open(args.output, "w", encoding="utf-8") as f:
        f.write(rule.join(sections) + "\n")


if __name__ == "__main__":
    main()
