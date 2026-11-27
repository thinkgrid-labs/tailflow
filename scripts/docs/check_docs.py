#!/usr/bin/env python3
"""Check TailFlow's public documentation against the repository.

Run from the repository root:

    python3 scripts/docs/check_docs.py              # links, routes, config keys
    python3 scripts/docs/check_docs.py --bin target/debug   # also CLI commands

Checks:

1. Relative links and image paths in public Markdown resolve, and `#anchors`
   name a real heading (GitHub's slug rules).
2. Public docs never link to `docs/internal/` (gitignored, never published).
3. Every API path written in backticks (`/api/...`, `/health`, `/auth/...`,
   `/events`) is registered with `.route(...)` in the daemon.
4. Every key in a ```toml block belongs to the matching config struct
   (`[sources]`, `[storage]`, `[privacy]`, `[observe]` ...), read from the
   Rust source so the list cannot go stale.
5. With --bin: every `tailflow`, `tailflow-daemon`, `tailflow-logs` and
   `tailflow-mcp` command in a code block names real subcommands, and its
   `--flags` appear in that subcommand's `--help`.

Standard library only. Exit status 1 when any check fails.
"""

from __future__ import annotations

import argparse
import re
import shlex
import subprocess
import sys
from functools import lru_cache
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

PUBLIC_DOCS = [
    "README.md",
    "CONTRIBUTING.md",
    "CHANGELOG.md",
    "npm/tailflow/README.md",
    *sorted(str(p.relative_to(ROOT)) for p in (ROOT / "docs").glob("*.md")),
    *sorted(str(p.relative_to(ROOT)) for p in (ROOT / ".github").rglob("*.md")),
]

# Config sections → (file, struct). Keys are the struct's serde field names.
CONFIG_STRUCTS = {
    "sources": ("crates/tailflow-core/src/config.rs", "SourcesConfig"),
    "sources.file": ("crates/tailflow-core/src/config.rs", "FileEntry"),
    "sources.process": ("crates/tailflow-core/src/config.rs", "ProcessEntry"),
    "storage": ("crates/tailflow-core/src/config.rs", "StorageConfig"),
    "privacy": ("crates/tailflow-core/src/privacy.rs", "PrivacyConfig"),
    "privacy.redaction_rules": ("crates/tailflow-core/src/privacy.rs", "RedactionRuleConfig"),
    "observe": ("crates/tailflow-core/src/observe/mod.rs", "ObserveConfig"),
}

BINARIES = ("tailflow", "tailflow-daemon", "tailflow-logs", "tailflow-mcp")

errors: list[str] = []


def fail(path: str, line: int, msg: str) -> None:
    errors.append(f"{path}:{line}: {msg}")


# ── Markdown helpers ─────────────────────────────────────────────────────────

FENCE = re.compile(r"^\s*(```|~~~)\s*([\w+-]*)")


def split_blocks(text: str):
    """Yield (kind, lang, start_line, lines): kind is 'code' or 'prose'."""
    kind, lang, start, buf = "prose", "", 1, []
    for no, line in enumerate(text.splitlines(), 1):
        m = FENCE.match(line)
        if m and kind == "prose":
            if buf:
                yield kind, lang, start, buf
            kind, lang, start, buf = "code", m.group(2).lower(), no + 1, []
        elif m and kind == "code":
            yield kind, lang, start, buf
            kind, lang, start, buf = "prose", "", no + 1, []
        else:
            buf.append(line)
    if buf:
        yield kind, lang, start, buf


def github_slug(heading: str) -> str:
    text = re.sub(r"<[^>]+>", "", heading)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)  # links → text
    text = text.replace("`", "").strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


@lru_cache(maxsize=None)
def anchors(path: Path) -> frozenset[str]:
    seen: dict[str, int] = {}
    out = set()
    for kind, _, _, lines in split_blocks(path.read_text(encoding="utf-8")):
        if kind != "prose":
            continue
        for line in lines:
            m = re.match(r"^#{1,6}\s+(.*?)\s*#*\s*$", line)
            if not m:
                continue
            slug = github_slug(m.group(1))
            n = seen.get(slug, 0)
            seen[slug] = n + 1
            out.add(slug if n == 0 else f"{slug}-{n}")
        for line in lines:
            for a in re.findall(r'<a\s+(?:name|id)="([^"]+)"', line):
                out.add(a)
    return frozenset(out)


LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)|!\[[^\]]*\]\(([^)\s]+)\)")
HTML_LINK = re.compile(r'(?:href|src)="([^"]+)"')


def check_links(rel: str) -> None:
    path = ROOT / rel
    for kind, _, start, lines in split_blocks(path.read_text(encoding="utf-8")):
        if kind != "prose":
            continue
        for off, line in enumerate(lines):
            no = start + off
            prose = re.sub(r"`[^`]*`", "", line)
            targets = [a or b for a, b in LINK.findall(prose)] + HTML_LINK.findall(prose)
            for target in targets:
                if re.match(r"^[a-z]+:", target) or target.startswith("//"):
                    if "docs/internal" in target:
                        fail(rel, no, f"links to private docs/internal: {target}")
                    continue
                file_part, _, anchor = target.partition("#")
                dest = (path.parent / file_part).resolve() if file_part else path
                if "docs/internal" in str(dest.relative_to(ROOT) if dest.is_relative_to(ROOT) else dest):
                    fail(rel, no, f"links to private docs/internal: {target}")
                    continue
                if not dest.exists():
                    fail(rel, no, f"broken link: {target}")
                    continue
                if anchor and dest.suffix == ".md" and anchor not in anchors(dest):
                    fail(rel, no, f"missing anchor #{anchor} in {dest.relative_to(ROOT)}")


# ── API routes ───────────────────────────────────────────────────────────────


def code_routes() -> set[str]:
    routes = set()
    for rs in (ROOT / "crates/tailflow-daemon/src").glob("*.rs"):
        src = rs.read_text(encoding="utf-8")
        routes.update(re.findall(r'\.route\(\s*"([^"]+)"', src))
    return {normalize_route(r) for r in routes}


def normalize_route(route: str) -> str:
    route = route.split("?", 1)[0].rstrip("/") or "/"
    route = re.sub(r"\{[^}]*\}|<[^>]*>|:[a-z_]+", "{}", route)
    return route


ROUTE_IN_TEXT = re.compile(r"`(?:GET |POST )?(/(?:api|health|auth|events)[^`\s]*)`")


def check_routes(rel: str, known: set[str]) -> None:
    text = (ROOT / rel).read_text(encoding="utf-8")
    for no, line in enumerate(text.splitlines(), 1):
        for raw in ROUTE_IN_TEXT.findall(line):
            route = normalize_route(raw)
            if route.endswith("/*") or route.endswith("…"):
                continue
            if route in known or any(k.startswith(route + "/") for k in known):
                continue  # an exact route, or a prefix such as `/api/v1`
            fail(rel, no, f"API path not registered in the daemon: {raw}")


# ── Config keys ──────────────────────────────────────────────────────────────


@lru_cache(maxsize=None)
def struct_fields(file: str, struct: str) -> frozenset[str]:
    src = (ROOT / file).read_text(encoding="utf-8")
    m = re.search(r"pub struct " + struct + r"\b[^{]*\{(.*?)\n\}", src, re.S)
    if not m:
        raise SystemExit(f"check_docs: struct {struct} not found in {file}")
    fields, rename = set(), None
    for line in m.group(1).splitlines():
        line = line.strip()
        r = re.search(r'rename\s*=\s*"([^"]+)"', line)
        if line.startswith("#[") and r:
            rename = r.group(1)
            continue
        for alias in re.findall(r'alias\s*=\s*"([^"]+)"', line):
            fields.add(alias)
        f = re.match(r"(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:", line)
        if f:
            fields.add(rename or f.group(1))
            rename = None
    return frozenset(fields)


def check_toml(rel: str) -> None:
    text = (ROOT / rel).read_text(encoding="utf-8")
    if rel.endswith(".toml"):
        blocks = [("code", "toml", 1, text.splitlines())]
    else:
        blocks = list(split_blocks(text))
    for kind, lang, start, lines in blocks:
        if kind != "code" or lang != "toml":
            continue
        section = None
        for off, line in enumerate(lines):
            no = start + off
            s = line.split("#", 1)[0].strip() if not line.strip().startswith("pattern") else line.strip()
            if not s:
                continue
            h = re.match(r"^\[\[?([a-z_.]+)\]\]?$", s)
            if h:
                section = h.group(1)
                if section not in CONFIG_STRUCTS:
                    fail(rel, no, f"unknown config section [{section}]")
                continue
            k = re.match(r"^([a-z_]+)\s*=", s)
            if k and section in CONFIG_STRUCTS:
                file, struct = CONFIG_STRUCTS[section]
                if k.group(1) not in struct_fields(file, struct):
                    fail(rel, no, f"[{section}] has no key `{k.group(1)}` ({struct})")


# ── CLI commands ─────────────────────────────────────────────────────────────


@lru_cache(maxsize=None)
def help_text(bindir: str, chain: tuple[str, ...]) -> str | None:
    exe = Path(bindir) / chain[0]
    if not exe.exists():
        exe = exe.with_suffix(".exe")
    try:
        out = subprocess.run(
            [str(exe), *chain[1:], "--help"],
            capture_output=True, text=True, timeout=20, stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return out.stdout + out.stderr if out.returncode == 0 else None


def subcommands(help_out: str) -> set[str]:
    m = re.search(r"^Commands:\n(.*?)(?:\n\n|\Z)", help_out, re.S | re.M)
    return set(re.findall(r"^\s{2}([a-z][a-z0-9-]*)", m.group(1), re.M)) if m else set()


SHELL_LANGS = {"bash", "sh", "shell", "console", "zsh", ""}


def check_cli(rel: str, bindir: str) -> None:
    text = (ROOT / rel).read_text(encoding="utf-8")
    for kind, lang, start, lines in split_blocks(text):
        if kind != "code" or lang not in SHELL_LANGS:
            continue
        for off, line in enumerate(lines):
            no = start + off
            cmd = line.split(" #", 1)[0].strip().lstrip("$ ").strip()
            for part in re.split(r"\s*(?:\||&&|;|\$\(|\))\s*", cmd):
                try:
                    words = shlex.split(part)
                except ValueError:
                    continue
                while words and re.match(r"^[A-Z_]+=", words[0]):
                    words = words[1:]  # VAR=value prefixes
                if not words or words[0] not in BINARIES:
                    continue
                check_command(rel, no, bindir, words)


def check_command(rel: str, no: int, bindir: str, words: list[str]) -> None:
    chain = (words[0],)
    out = help_text(bindir, chain)
    if out is None:
        fail(rel, no, f"cannot run {words[0]} --help in {bindir}")
        return
    rest = words[1:]
    while rest and not rest[0].startswith("-") and rest[0] in subcommands(out):
        chain += (rest.pop(0),)
        out = help_text(bindir, chain)
        if out is None:
            fail(rel, no, f"`{' '.join(chain)} --help` failed")
            return
    usage = next((l for l in out.splitlines() if l.startswith("Usage:")), "")
    positionals = set(re.findall(r"<([A-Z_]+)>", usage)) - {"COMMAND"}
    if rest and not rest[0].startswith("-") and subcommands(out) and not positionals:
        fail(rel, no, f"`{' '.join(chain)}` has no subcommand `{rest[0]}`")
    for w in rest:
        if not w.startswith("--"):
            continue
        flag = w.split("=", 1)[0]
        if not re.search(r"(?<![\w-])" + re.escape(flag) + r"(?![\w-])", out):
            fail(rel, no, f"`{' '.join(chain)}` has no flag {flag}")


# ── main ─────────────────────────────────────────────────────────────────────


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", help="directory with built tailflow binaries; enables the CLI check")
    args = ap.parse_args()

    known = code_routes()
    docs = [d for d in PUBLIC_DOCS if (ROOT / d).exists()]
    for rel in docs:
        check_links(rel)
        if rel != "CHANGELOG.md":  # the changelog records history, including removed paths
            check_routes(rel, known)
        check_toml(rel)
        if args.bin:
            check_cli(rel, args.bin)
    check_toml("tailflow.example.toml")

    for e in errors:
        print(e)
    checks = "links, routes, config keys" + (", CLI" if args.bin else "")
    print(f"check_docs: {len(docs)} files, {checks}: {len(errors)} problem(s)")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
