"""Render gateway configs: @@TOKEN@@ substitution, @@REPEAT@@ blocks, @@EACH_n@@ file fan-out."""
from __future__ import annotations

import re
from pathlib import Path

TOKEN = re.compile(r"@@([A-Z][A-Z0-9_]*)@@")
REPEAT = re.compile(r"^\s*@@REPEAT (\d+)(?: SEP (\S+))?@@\s*$")
END = re.compile(r"^\s*@@END@@\s*$")
EACH = re.compile(r"@@EACH_(\d+)@@")


class TemplateError(Exception):
    """A config template is malformed or references an unknown token."""


def expand_repeats(text: str) -> str:
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    while i < len(lines):
        m = REPEAT.match(lines[i])
        if not m:
            if END.match(lines[i]):
                raise TemplateError(f"line {i + 1}: @@END@@ without @@REPEAT@@")
            out.append(lines[i])
            i += 1
            continue
        n, sep = int(m.group(1)), m.group(2) or ""
        j = i + 1
        while j < len(lines) and not END.match(lines[j]):
            if REPEAT.match(lines[j]):
                raise TemplateError(f"line {j + 1}: nested @@REPEAT@@ is not supported")
            j += 1
        if j == len(lines):
            raise TemplateError(f"line {i + 1}: @@REPEAT@@ without @@END@@")
        body = "".join(lines[i + 1:j])
        width = len(str(n - 1))
        copies = [body.replace("@@I@@", str(k).zfill(width)) for k in range(n)]
        if sep:
            copies = [c.rstrip("\n") + (sep if k < n - 1 else "") + "\n" for k, c in enumerate(copies)]
        out.extend(copies)
        i = j + 1
    return "".join(out)


def render_text(text: str, values: dict[str, str]) -> str:
    text = expand_repeats(text.replace("\r\n", "\n"))
    out: list[str] = []
    for lineno, line in enumerate(text.splitlines(keepends=True), 1):
        alone = TOKEN.fullmatch(line.strip())
        if alone and "\n" in values.get(alone.group(1), ""):
            indent = line[: len(line) - len(line.lstrip())]
            block = values[alone.group(1)].rstrip("\n").split("\n")
            out.append("".join(f"{indent}{b}\n" for b in block))
            continue

        def sub(m: re.Match) -> str:
            name = m.group(1)
            if name not in values:
                raise TemplateError(f"line {lineno}: unknown token @@{name}@@")
            if "\n" in values[name]:
                raise TemplateError(f"line {lineno}: multi-line @@{name}@@ must stand alone on its line")
            return values[name]

        out.append(TOKEN.sub(sub, line))
    return "".join(out)


def render_dir(src: Path, dst: Path, values: dict[str, str]) -> list[Path]:
    """Render every file under src into dst (merging into whatever dst already holds)."""
    written: list[Path] = []
    for f in sorted(src.rglob("*")):
        if f.is_dir():
            continue
        rel = f.relative_to(src).as_posix()
        m = EACH.search(rel)
        if m:
            n = int(m.group(1))
            width = len(str(n - 1))
            variants = [(EACH.sub(str(k).zfill(width), rel), {**values, "I": str(k).zfill(width)})
                        for k in range(n)]
        else:
            variants = [(rel, values)]
        raw = f.read_bytes()
        for relpath, vals in variants:
            out = dst / relpath
            out.parent.mkdir(parents=True, exist_ok=True)
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                out.write_bytes(raw)
            else:
                try:
                    rendered = render_text(text, vals)
                except TemplateError as e:
                    raise TemplateError(f"{src.name}/{rel}: {e}") from None
                out.write_bytes(rendered.encode("utf-8"))
            written.append(out)
    return written
