"""Convert Project Gutenberg plain texts to the web emulator's book markup.

Markup: '# ' chapter heading, '~ ' centered verse line, blank-line
paragraphs. Normalizes typography the 1-bit Literata bitmaps may lack.
"""

import re
import sys

OUT = sys.argv[2] if len(sys.argv) > 2 else "."
SRC = sys.argv[1] if len(sys.argv) > 1 else "."

ROMAN = {
    "I": "One", "II": "Two", "III": "Three", "IV": "Four", "V": "Five",
    "VI": "Six", "VII": "Seven", "VIII": "Eight", "IX": "Nine", "X": "Ten",
    "XI": "Eleven", "XII": "Twelve",
}


SMALL_WORDS = {"a", "an", "and", "of", "the", "in", "on", "his", "her", "to", "with", "at", "by", "for"}


def title_case(text: str) -> str:
    words = text.lower().split()
    out = []
    for index, word in enumerate(words):
        if index > 0 and word in SMALL_WORDS:
            out.append(word)
        else:
            out.append(word[:1].upper() + word[1:])
    return " ".join(out)


def normalize(text: str) -> str:
    for a, b in [
        ("“", '"'), ("”", '"'), ("‘", "'"), ("’", "'"),
        ("Æ", "Ae"), ("æ", "ae"), ("Œ", "Oe"), ("œ", "oe"),
        ("…", "..."), ("–", "-"), (" ", " "),
        ("_", ""),  # PG italics markers
    ]:
        text = text.replace(a, b)
    return text


def body(path: str) -> list[str]:
    raw = open(path, encoding="utf-8").read()
    raw = raw.split("***", 2)[2]          # after START sentinel
    raw = raw.rsplit("*** END", 1)[0]
    return normalize(raw).splitlines()


def flush(par: list[str], out: list[str]) -> None:
    if par:
        out.append(" ".join(word for word in " ".join(par).split()))
        out.append("")
        par.clear()


def emit_stream(lines: list[str], out: list[str], par: list[str]) -> None:
    """Paragraphs + indented lines as centered verse."""
    for line in lines:
        if not line.strip():
            flush(par, out)
        elif re.match(r"^\s{4,}\S", line):
            flush(par, out)
            out.append("~ " + line.strip())
        else:
            par.append(line.strip())
    flush(par, out)


def alice() -> None:
    lines = body(f"{SRC}/alice.txt")
    out: list[str] = []
    par: list[str] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        match = re.match(r"^CHAPTER ([IVX]+)\.\s*$", line)
        if match:
            flush(par, out)
            title = lines[i + 1].strip()
            out.append(f"# {ROMAN[match.group(1)]}. {title}")
            out.append("")
            i += 2
            continue
        if not line.strip():
            flush(par, out)
        elif re.match(r"^\s{4,}\S", line):
            flush(par, out)
            out.append("~ " + line.strip())
        else:
            par.append(line.strip())
        i += 1
    flush(par, out)
    text = "\n".join(out)
    text = text.split("# One.", 1)[1]
    open(f"{OUT}/alice.txt", "w").write("# One." + text.rstrip() + "\n")


def carol() -> None:
    lines = body(f"{SRC}/carol.txt")
    out: list[str] = []
    par: list[str] = []
    for line in lines:
        match = re.match(r"^STAVE ([IVX]+):\s*(.+)$", line)
        if match:
            flush(par, out)
            out.append(f"# Stave {ROMAN[match.group(1)]}: {title_case(match.group(2))}")
            out.append("")
        elif not line.strip():
            flush(par, out)
        elif re.match(r"^\s{6,}\S", line):
            flush(par, out)
            out.append("~ " + line.strip())
        else:
            par.append(line.strip())
    flush(par, out)
    text = "\n".join(out)
    text = "# Stave" + text.split("# Stave", 1)[1]
    open(f"{OUT}/carol.txt", "w").write(text.rstrip() + "\n")


def aesop(count: int = 40) -> None:
    lines = body(f"{SRC}/aesop.txt")
    # Table-of-contents titles appear early, one per line, indented one space.
    toc: list[str] = []
    for line in lines[:400]:
        match = re.match(r"^ (The .+|[A-Z][a-z].+)$", line)
        if match and len(match.group(1)) < 60:
            toc.append(match.group(1).strip().lower())
    titles = set(toc)

    out: list[str] = []
    par: list[str] = []
    fables = 0
    started = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        is_title = (
            stripped.lower() in titles
            and index > 700                      # past the TOC and front matter
            and index + 1 < len(lines)
            and not lines[index + 1].strip()     # blank after a heading
            and not line.startswith(" ")
        )
        if is_title:
            if fables >= count:
                break
            flush(par, out)
            out.append(f"# {title_case(stripped)}")
            out.append("")
            fables += 1
            started = True
        elif started:
            if not stripped:
                flush(par, out)
            else:
                par.append(stripped)
    flush(par, out)
    open(f"{OUT}/aesop.txt", "w").write("\n".join(out).rstrip() + "\n")
    print(f"aesop: {fables} fables")


alice()
carol()
aesop()
for name in ["alice", "carol", "aesop"]:
    text = open(f"{OUT}/{name}.txt").read()
    chapters = text.count("\n# ") + text.startswith("# ")
    print(name, len(text), "bytes,", chapters, "chapters")
