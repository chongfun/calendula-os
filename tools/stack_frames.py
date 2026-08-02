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
import unittest

# Bytes. The largest legitimate frame today is ensure_epub_scratch at 20,960 --
# the inflate state that miniz_oxide can only build by value. This sits far
# enough above that to leave room for honest drift, and far enough below the
# 42,136-byte X3 stack that a frame reaching it is worth a human deciding
# whether the call chain beneath it still fits.
DEFAULT_BUDGET = 24 * 1024

# Non-Rust assembly entry points and pre-compiled vendor SDK blobs whose stack
# adjustment cannot be bounded as a constant by static analysis:
# - `_abs_start`, `_pre_default_start_trap`, `swint_handler_trampoline`:
#   Low-level boot and exception trap handlers in `esp-riscv-rt` that manipulate
#   `sp` using CSRs or absolute address loads (`auipc`).
# - `ppCalTkipMic`, `ppTxFragmentProc`: Pre-compiled Espressif Wi-Fi SDK blobs
#   in `libpp.a` that use dynamic variable-length stack allocations (VLAs) at
#   runtime.
EXEMPT_UNRESOLVED = {
    "_abs_start",
    "_pre_default_start_trap",
    "ppCalTkipMic",
    "ppTxFragmentProc",
    "swint_handler_trampoline",
}

FUNC_RE = re.compile(r"^[0-9a-f]+\s+<(?P<name>[^.\s<][^<>]*)>:\s*$")
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


def find_nm() -> str:
    if os.environ.get("LLVM_NM"):
        return os.environ["LLVM_NM"]
    return find_objdump().replace("llvm-objdump", "llvm-nm")


def imm(token: str) -> int:
    token = token.strip().rstrip(",")
    if not IMM_RE.match(token):
        raise ValueError(token)
    neg = token.startswith("-")
    body = token[1:] if neg else token
    value = int(body, 16) if body.startswith("0x") else int(body)
    return -value if neg else value


def parse_insn_line(line: str) -> tuple[str, list[str]] | None:
    if ":" not in line:
        return None
    colon_idx = line.find(":")
    addr_part = line[:colon_idx].strip()
    if not addr_part.isalnum():
        return None
    rest = line[colon_idx + 1 :]
    if "\t" in rest:
        parts = [p.strip() for p in rest.split("\t") if p.strip()]
        if len(parts) >= 2:
            mnem = parts[1]
            ops_str = parts[2] if len(parts) > 2 else ""
            ops = [p.strip() for p in ops_str.split(",") if p.strip()]
            return mnem, ops
    insn = INSN_RE.match(line)
    if insn:
        ops = [part.strip() for part in insn.group("ops").split(",") if part.strip()]
        return insn.group("mnem"), ops
    return None


def frame_size(instructions: list[tuple[str, list[str]]]) -> int | None:
    """Peak bytes this function subtracts from `sp`.

    Tracks `sp` through the straight-line instruction stream, following the
    small constant materialisations RISC-V needs for frames over 2 KB: `lui`
    plus `addi` into a scratch register, then `sub sp, sp, reg`. Positive
    adjustments are treated as deallocation so an epilogue does not inflate the
    peak. Branches are ignored -- a frame this tool cares about is allocated in
    the prologue, and a loop that grew `sp` without bound would be a different
    bug.

    Returns `None` if any instruction writes to `sp` in a way that cannot be
    resolved as a constant stack allocation, or uses an untracked register,
    failing closed rather than silently ignoring the stack modification.
    """
    regs: dict[str, int] = {}
    cur = peak = 0
    for mnem, ops in instructions:
        if not ops:
            continue
        if ops[0] == "sp":
            if mnem == "addi" and len(ops) == 3 and ops[1] == "sp":
                try:
                    delta = imm(ops[2])
                except ValueError:
                    # A relocation or symbolic operand on a write to sp. It
                    # cannot be bounded, and skipping it would under-report the
                    # frame -- the one direction this tool must never fail in.
                    return None
                cur -= delta
            elif mnem == "sub" and len(ops) == 3 and ops[1] == "sp":
                if ops[2] not in regs:
                    return None
                cur += regs[ops[2]]
            elif mnem == "add" and len(ops) == 3 and ops[1] == "sp":
                if ops[2] not in regs:
                    return None
                cur -= regs[ops[2]]
            else:
                # Any unmodeled instruction modifying sp (e.g. mv sp, t0, addi sp, s0, N)
                # cannot be bounded as a frame constant: fail closed.
                return None
        elif mnem == "lui" and len(ops) == 2:
            # An unresolvable immediate must *invalidate* the destination, not
            # leave the previous constant in place: a later `sub sp, sp, rd`
            # would otherwise consume a stale value and report a confidently
            # wrong frame instead of failing closed.
            try:
                regs[ops[0]] = imm(ops[1]) << 12
            except ValueError:
                regs.pop(ops[0], None)
        elif mnem == "addi" and len(ops) == 3:
            if ops[1] in regs:
                try:
                    regs[ops[0]] = regs[ops[1]] + imm(ops[2])
                except ValueError:
                    regs.pop(ops[0], None)
            else:
                regs.pop(ops[0], None)
        elif mnem == "mv" and len(ops) == 2:
            if ops[1] in regs:
                regs[ops[0]] = regs[ops[1]]
            else:
                regs.pop(ops[0], None)
        else:
            # Any unmodeled instruction that writes to a register invalidates
            # any previously tracked constant for that register so a subsequent
            # sp adjustment cannot consume a stale value.
            regs.pop(ops[0], None)
        peak = max(peak, cur)
    return peak


def parse(disassembly: str) -> dict[str, int | None]:
    frames: dict[str, int | None] = {}
    name: str | None = None
    body: list[tuple[str, list[str]]] = []
    for line in disassembly.splitlines():
        header = FUNC_RE.match(line)
        if header:
            if name is not None:
                frames[name] = frame_size(body)
            name, body = header.group("name"), []
            continue
        insn = parse_insn_line(line)
        if insn and name is not None:
            body.append(insn)
    if name is not None:
        frames[name] = frame_size(body)
    return frames


def stack_region(elf: str) -> int | None:
    """`_stack_start - _stack_end`, for context in the report."""
    nm = find_nm()
    try:
        out = subprocess.run(
            [nm, elf], capture_output=True, text=True, check=True, timeout=30
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError, subprocess.TimeoutExpired):
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
    ap.add_argument(
        "--budget", type=int, default=int(os.environ.get("STACK_FRAME_BUDGET", DEFAULT_BUDGET))
    )
    ap.add_argument("--top", type=int, default=5)
    args = ap.parse_args()

    with open(args.binary, "rb") as handle:
        is_elf = handle.read(4) == b"\x7fELF"

    if is_elf:
        objdump = find_objdump()
        try:
            disassembly = subprocess.run(
                [objdump, "-d", args.binary], capture_output=True, text=True, check=True, timeout=30
            ).stdout
        except (subprocess.CalledProcessError, FileNotFoundError, subprocess.TimeoutExpired) as err:
            print(
                f"error: failed to run {objdump}: {err}\n"
                "Install it with:\n"
                "  rustup component add llvm-tools\n"
                "or point LLVM_OBJDUMP at one.",
                file=sys.stderr,
            )
            return 2
        region = stack_region(args.binary)
    else:
        with open(args.binary) as handle:
            disassembly = handle.read()
        region = None

    frames = parse(disassembly)
    if not frames:
        print(f"error: no functions found in {args.binary}", file=sys.stderr)
        return 2

    unresolved = [
        name for name, size in frames.items() if size is None and name not in EXEMPT_UNRESOLVED
    ]
    if unresolved:
        print(
            f"\nerror: could not resolve stack adjustment for {len(unresolved)} function(s):\n"
            + "\n".join(f"  {name}" for name in unresolved[:10])
            + ("\n  ..." if len(unresolved) > 10 else "")
            + "\nAn instruction modified sp with an untracked register or unmodeled operation.\n"
            "Fail closed to prevent uncaught stack overflows on device.",
            file=sys.stderr,
        )
        return 1

    exempt = [name for name, size in frames.items() if size is None and name in EXEMPT_UNRESOLVED]
    valid_frames: dict[str, int] = {k: v for k, v in frames.items() if v is not None}
    ranked = sorted(valid_frames.items(), key=lambda kv: kv[1], reverse=True)
    label = f"stack region {region} B, " if region is not None else ""
    exempt_label = f" ({len(exempt)} exempt unresolved)" if exempt else ""
    print(f"  {label}budget {args.budget} B, {len(valid_frames)} functions{exempt_label}")
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


class TestStackFrames(unittest.TestCase):
    def test_direct_small_frame(self) -> None:
        insns = [("addi", ["sp", "sp", "-32"])]
        self.assertEqual(frame_size(insns), 32)

    def test_large_frame_lui_addi_sub(self) -> None:
        insns = [
            ("lui", ["t0", "5"]),
            ("addi", ["t0", "t0", "1024"]),
            ("sub", ["sp", "sp", "t0"]),
        ]
        self.assertEqual(frame_size(insns), 21504)

    def test_epilogue_does_not_increase_peak(self) -> None:
        insns = [
            ("addi", ["sp", "sp", "-64"]),
            ("addi", ["sp", "sp", "64"]),
        ]
        self.assertEqual(frame_size(insns), 64)

    def test_untracked_register_fails_closed(self) -> None:
        insns = [("sub", ["sp", "sp", "a5"])]
        self.assertIsNone(frame_size(insns))

    def test_unknown_sp_write_fails_closed(self) -> None:
        insns = [("mv", ["sp", "t0"])]
        self.assertIsNone(frame_size(insns))

    def test_untracked_overwritten_register_fails_closed(self) -> None:
        insns = [
            ("lui", ["t0", "5"]),
            ("lw", ["t0", "0(a0)"]),
            ("sub", ["sp", "sp", "t0"]),
        ]
        self.assertIsNone(frame_size(insns))

    def test_unknown_add_sp_fails_closed(self) -> None:
        insns = [("add", ["sp", "sp", "a5"])]
        self.assertIsNone(frame_size(insns))

    def test_tracked_add_sp_restores_frame(self) -> None:
        insns = [
            ("lui", ["t0", "5"]),
            ("addi", ["t0", "t0", "1024"]),
            ("sub", ["sp", "sp", "t0"]),
            ("add", ["sp", "sp", "t0"]),
        ]
        self.assertEqual(frame_size(insns), 21504)

    # The tab-separated form is what llvm-objdump actually emits, so it is the
    # branch that runs against every real binary -- while the space-separated
    # cases below only reach the INSN_RE fallback. These three lines are copied
    # verbatim out of an `llvm-objdump -d` of the X3 release build.
    def test_parse_real_objdump_line(self) -> None:
        line = "40380190: 94410113     \taddi\tsp, sp, -0x6bc"
        self.assertEqual(parse_insn_line(line), ("addi", ["sp", "sp", "-0x6bc"]))

    def test_parse_real_objdump_register_operand(self) -> None:
        line = "42316464: 40b10133     \tsub\tsp, sp, a1"
        self.assertEqual(parse_insn_line(line), ("sub", ["sp", "sp", "a1"]))

    def test_parse_real_compressed_line_without_operands(self) -> None:
        # RVC, two bytes, no operand field at all.
        self.assertEqual(parse_insn_line("40380698: 8082         \tret"), ("ret", []))

    def test_parse_real_objdump_frame_round_trip(self) -> None:
        # The two forms must agree end to end, since the whole tool rests on it.
        lines = [
            "4231644a: 7111         \taddi\tsp, sp, -0x100",
            "4231645e: 6595         \tlui\ta1, 0x5",
            "42316460: 0e058593     \taddi\ta1, a1, 0xe0",
            "42316464: 40b10133     \tsub\tsp, sp, a1",
        ]
        insns = [parse_insn_line(line) for line in lines]
        self.assertNotIn(None, insns)
        self.assertEqual(frame_size(insns), 0x100 + 0x50E0)

    def test_unresolvable_sp_immediate_fails_closed(self) -> None:
        # Skipping this would under-report the frame, the one unsafe direction.
        insns = [("addi", ["sp", "sp", "%lo(x)"])]
        self.assertIsNone(frame_size(insns))

    def test_unresolvable_lui_invalidates_stale_constant(self) -> None:
        # Without the invalidation this reported 20480 -- a confidently wrong
        # frame built from a register the failed `lui` was meant to overwrite.
        insns = [
            ("lui", ["t0", "0x5"]),
            ("lui", ["t0", "%hi(x)"]),
            ("sub", ["sp", "sp", "t0"]),
        ]
        self.assertIsNone(frame_size(insns))

    def test_unresolvable_addi_invalidates_stale_constant(self) -> None:
        insns = [
            ("lui", ["t0", "0x5"]),
            ("addi", ["t0", "t0", "%lo(x)"]),
            ("sub", ["sp", "sp", "t0"]),
        ]
        self.assertIsNone(frame_size(insns))

    def test_parse_multiple_functions(self) -> None:
        disasm = (
            "00000000 <func_a>:\n"
            "   0: 71 71       addi sp, sp, -16\n"
            "00000004 <func_b>:\n"
            "   4: 37 55 00 00 lui t0, 5\n"
            "   8: 13 05 05 40 addi t0, t0, 1024\n"
            "   c: 33 81 51 40 sub sp, sp, t0\n"
        )
        parsed = parse(disasm)
        self.assertEqual(parsed, {"func_a": 16, "func_b": 21504})


if __name__ == "__main__":
    sys.exit(main())
