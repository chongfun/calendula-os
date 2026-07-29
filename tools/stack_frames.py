#!/usr/bin/env python3
"""Fail the build when one function's stack frame gets too big.

The device has no stack guard page. An overflow runs off the bottom of the
stack straight into `.bss`, corrupts whatever static lives there, and surfaces
later as an unrelated panic -- esp-hal's clock singleton unwrapping a `None`,
say, several calls after the function that actually overflowed. Nothing in the
build catches it, and it costs a flash-and-reproduce to find.

The regression this exists to catch was 28 bytes of struct growth. That was
enough to stop LLVM forwarding an `sret` slot into a static, so a 32 KB
decompression window that had always been constructed in place started being
built on the stack first: `ensure_epub_scratch` went from 20,976 to 53,744
bytes against a 42,136-byte stack. The source diff that did it touched none of
the code involved, which is exactly why a human reading the diff will not see
it and a number checked by a machine will.

This is a per-function bound, not a call-depth analysis -- it cannot prove the
whole chain fits. It is a tripwire on the one shape that has actually bitten:
a single frame quietly acquiring a multi-kilobyte temporary.

Usage:
    tools/stack_frames.py <fw.elf | disassembly.txt> [--budget BYTES]

A pre-dumped `llvm-objdump -d` listing is accepted in place of an ELF so an
already-built binary can be checked without a toolchain.
"""

from __future__ import annotations

import argparse
import glob
import os
import re
import subprocess
import sys

# Bytes. The largest legitimate frame today is ensure_epub_scratch at 20,960 --
# the inflate state that miniz_oxide can only build by value. This sits far
# enough above that to leave room for honest drift, and far enough below the
# 42,136-byte X3 stack that a frame reaching it is worth a human deciding
# whether the call chain beneath it still fits.
DEFAULT_BUDGET = 24 * 1024

FUNC_RE = re.compile(r"^[0-9a-f]+\s+<(?P<name>.+)>:\s*$")
INSN_RE = re.compile(r"^\s*[0-9a-f]+:\s+(?:[0-9a-f]{2,8}\s+)+\s*(?P<mnem>\S+)\s*(?P<ops>.*)$")
IMM_RE = re.compile(r"^-?0x[0-9a-f]+$|^-?\d+$")


def find_objdump() -> str:
    if os.environ.get("LLVM_OBJDUMP"):
        return os.environ["LLVM_OBJDUMP"]
    rustup = os.environ.get("RUSTUP_HOME") or os.path.expanduser("~/.rustup")
    hits = sorted(glob.glob(f"{rustup}/toolchains/*/lib/rustlib/*/bin/llvm-objdump"))
    if hits:
        return hits[-1]
    return "llvm-objdump"


def imm(token: str) -> int:
    token = token.strip().rstrip(",")
    if not IMM_RE.match(token):
        raise ValueError(token)
    neg = token.startswith("-")
    body = token[1:] if neg else token
    value = int(body, 16) if body.startswith("0x") else int(body)
    return -value if neg else value


def frame_size(instructions: list[tuple[str, list[str]]]) -> int:
    """Peak bytes this function subtracts from `sp`.

    Tracks `sp` through the straight-line instruction stream, following the
    small constant materialisations RISC-V needs for frames over 2 KB: `lui`
    plus `addi` into a scratch register, then `sub sp, sp, reg`. Positive
    adjustments are treated as deallocation so an epilogue does not inflate the
    peak. Branches are ignored -- a frame this tool cares about is allocated in
    the prologue, and a loop that grew `sp` without bound would be a different
    bug.
    """
    regs: dict[str, int] = {}
    cur = peak = 0
    for mnem, ops in instructions:
        try:
            if mnem == "lui" and len(ops) == 2:
                regs[ops[0]] = imm(ops[1]) << 12
            elif mnem == "addi" and len(ops) == 3:
                if ops[0] == "sp" and ops[1] == "sp":
                    cur -= imm(ops[2])
                else:
                    regs[ops[0]] = regs.get(ops[1], 0) + imm(ops[2])
            elif mnem in ("sub", "add") and len(ops) == 3 and ops[0] == "sp" and ops[1] == "sp":
                delta = regs.get(ops[2], 0)
                cur += delta if mnem == "sub" else -delta
            elif mnem == "mv" and len(ops) == 2:
                regs[ops[0]] = regs.get(ops[1], 0)
        except ValueError:
            # A relocation or symbolic operand; it cannot be a frame constant.
            continue
        peak = max(peak, cur)
    return peak


def parse(disassembly: str) -> dict[str, int]:
    frames: dict[str, int] = {}
    name: str | None = None
    body: list[tuple[str, list[str]]] = []
    for line in disassembly.splitlines():
        header = FUNC_RE.match(line)
        if header:
            if name is not None:
                frames[name] = frame_size(body)
            name, body = header.group("name"), []
            continue
        insn = INSN_RE.match(line)
        if insn and name is not None:
            ops = [part.strip() for part in insn.group("ops").split(",") if part.strip()]
            body.append((insn.group("mnem"), ops))
    if name is not None:
        frames[name] = frame_size(body)
    return frames


def stack_region(elf: str) -> int | None:
    """`_stack_start - _stack_end`, for context in the report."""
    nm = find_objdump().replace("llvm-objdump", "llvm-nm")
    try:
        out = subprocess.run([nm, elf], capture_output=True, text=True, check=True).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    marks = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[2] in ("_stack_start", "_stack_end"):
            marks[parts[2]] = int(parts[0], 16)
    if len(marks) == 2:
        return marks["_stack_start"] - marks["_stack_end"]
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary", help="firmware ELF, or a pre-dumped llvm-objdump listing")
    ap.add_argument("--budget", type=int, default=int(os.environ.get("STACK_FRAME_BUDGET", DEFAULT_BUDGET)))
    ap.add_argument("--top", type=int, default=5)
    args = ap.parse_args()

    with open(args.binary, "rb") as handle:
        is_elf = handle.read(4) == b"\x7fELF"

    if is_elf:
        objdump = find_objdump()
        try:
            disassembly = subprocess.run(
                [objdump, "-d", args.binary], capture_output=True, text=True, check=True
            ).stdout
        except FileNotFoundError:
            print(
                f"error: {objdump} not found. Install it with:\n"
                "  rustup component add llvm-tools\n"
                "or point LLVM_OBJDUMP at one.",
                file=sys.stderr,
            )
            return 2
        region = stack_region(args.binary)
    else:
        with open(args.binary, "r") as handle:
            disassembly = handle.read()
        region = None

    frames = parse(disassembly)
    if not frames:
        print(f"error: no functions found in {args.binary}", file=sys.stderr)
        return 2

    ranked = sorted(frames.items(), key=lambda kv: kv[1], reverse=True)
    label = "stack region {} B, ".format(region) if region else ""
    print(f"  {label}budget {args.budget} B, {len(frames)} functions")
    for name, size in ranked[: args.top]:
        print(f"  {size:>8} B  {name}")

    worst_name, worst = ranked[0]
    if worst > args.budget:
        sys.stdout.flush()  # so the report above precedes the error in CI logs
        print(
            f"\nerror: {worst_name} allocates {worst} B, over the {args.budget} B budget.\n"
            "A frame this size overflows into .bss on the device, which surfaces as an\n"
            "unrelated panic far from the cause. Shrink it -- most often by holding a\n"
            "large value behind a reference to a static rather than by value -- or raise\n"
            "DEFAULT_BUDGET in this file with a note saying why the call chain still fits.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
