#!/usr/bin/env python3.14
"""Enforce crate and chip-feature dependency boundaries across host, C3, and S3.

Checks Cargo's resolved dependency graphs (``cargo metadata --format-version 1
--locked``) along active ``normal`` and ``build`` edges (explicitly excluding
``dev`` dependencies) for:

1. Shared crates (``app-core``, ``display``, ``ui``, ``proto``, ``upload-store``,
   ``reader-cache``, ``storage``, ``fw-common``, ``hal-ext``) on the host
   target and as transitive subgraphs inside ``fw-c3`` and ``fw-s3``:
   - zero packages matching ``esp-*``, ``esp32*``, or ``xtensa-*``
   - zero enabled features in ``{esp32c3, esp32s3, unsafe-assume-single-core}``
2. ``fw-c3`` (``riscv32imc-unknown-none-elf``, default and ``--features device-x3``):
   - zero packages matching ``fw-s3``, ``esp32s3``, or ``xtensa-*``
   - zero enabled ``esp32s3`` features
3. ``fw-s3`` (unfiltered metadata in ``--root-only``/default mode, and
   ``--filter-platform xtensa-esp32s3-none-elf`` via ``tools/cargo-s3.sh`` when
   the S3 toolchain is installed or ``--require-s3`` is passed):
   - zero packages matching ``fw-c3``, ``esp32c3``, or ``esp-riscv-rt``
   - zero enabled features in ``{esp32c3, unsafe-assume-single-core}``
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import subprocess
import sys
import unittest
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parent.parent
EXPORT_ESP_PATH = REPO_ROOT / "target" / "toolchains" / "export-esp.sh"

SHARED_CRATES: tuple[str, ...] = (
    "app-core",
    "display",
    "ui",
    "proto",
    "upload-store",
    "reader-cache",
    "storage",
    "fw-common",
    "hal-ext",
)
SHARED_FORBIDDEN_PREFIXES: tuple[str, ...] = ("esp-", "esp32", "xtensa-")
SHARED_FORBIDDEN_FEATURES: frozenset[str] = frozenset(
    {"esp32c3", "esp32s3", "unsafe-assume-single-core"}
)

C3_TARGET = "riscv32imc-unknown-none-elf"
C3_FORBIDDEN_EXACT: frozenset[str] = frozenset({"fw-s3", "esp32s3"})
C3_FORBIDDEN_PREFIXES: tuple[str, ...] = ("xtensa-",)
C3_FORBIDDEN_FEATURES: frozenset[str] = frozenset({"esp32s3"})

S3_TARGET = "xtensa-esp32s3-none-elf"
S3_FORBIDDEN_EXACT: frozenset[str] = frozenset({"fw-c3", "esp32c3", "esp-riscv-rt"})
S3_FORBIDDEN_FEATURES: frozenset[str] = frozenset({"esp32c3", "unsafe-assume-single-core"})


@dataclass(frozen=True)
class BoundaryViolation:
    """Single dependency or feature boundary failure with its shortest path."""

    scope: str
    message: str
    path: tuple[str, ...]


def _activated_optional_aliases(pkg: dict[str, Any], node: dict[str, Any]) -> set[str]:
    """Return optional dependency aliases actually enabled by ``node['features']``."""
    specs = pkg.get("dependencies", [])
    opt_aliases = {
        (dep.get("rename") or dep["name"])
        for dep in specs
        if dep.get("optional", False) and dep.get("kind") in (None, "build")
    }
    if not opt_aliases:
        return set()

    enabled = set(node.get("features", []))
    activated = enabled & opt_aliases
    pkg_features = pkg.get("features", {})
    for feat in enabled:
        for item in pkg_features.get(feat, []):
            if item.startswith("dep:"):
                alias = item[4:]
                if alias in opt_aliases:
                    activated.add(alias)
            elif "/" in item:
                lhs = item.split("/", 1)[0]
                if not lhs.endswith("?") and lhs in opt_aliases:
                    activated.add(lhs)
            elif item in opt_aliases:
                activated.add(item)
    return activated


def _is_edge_active(
    pkg: dict[str, Any],
    node: dict[str, Any],
    rdep: dict[str, Any],
    dep_pkg: dict[str, Any],
    *,
    exclude_riscv32_target_edges: bool = False,
) -> bool:
    """Return True if ``rdep`` is an active normal or build dependency edge."""
    dep_kinds = rdep.get("dep_kinds", [])
    nb_kinds = [dk for dk in dep_kinds if dk.get("kind") in (None, "build")]
    if not nb_kinds:
        return False

    if exclude_riscv32_target_edges and all(
        dk.get("target") == 'cfg(target_arch = "riscv32")' for dk in nb_kinds
    ):
        return False

    # Inspect manifest dependency declarations so weak feature edges like
    # ``esp32s3?/rt`` in ``esp-hal`` do not count when ``dep:esp32s3`` is off.
    specs = [
        dep
        for dep in pkg.get("dependencies", [])
        if dep["name"] == dep_pkg["name"] and dep.get("kind") in (None, "build")
    ]
    if not specs:
        return True
    if any(not dep.get("optional", False) for dep in specs):
        return True

    aliases = {(dep.get("rename") or dep["name"]) for dep in specs}
    return bool(aliases & _activated_optional_aliases(pkg, node))


def _walk_graph(
    metadata: dict[str, Any],
    root_id: str,
    *,
    exclude_riscv32_target_edges: bool = False,
) -> dict[str, tuple[str, ...]]:
    """Return ``{pkg_id: path_of_names}`` reachable via normal/build edges."""
    pkgs = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}

    paths: dict[str, tuple[str, ...]] = {root_id: (pkgs[root_id]["name"],)}
    queue: list[str] = [root_id]
    idx = 0
    while idx < len(queue):
        nid = queue[idx]
        idx += 1
        pkg = pkgs[nid]
        node = nodes[nid]
        cur_path = paths[nid]
        for rdep in node.get("deps", []):
            cid = rdep["pkg"]
            if cid in paths:
                continue
            if _is_edge_active(
                pkg,
                node,
                rdep,
                pkgs[cid],
                exclude_riscv32_target_edges=exclude_riscv32_target_edges,
            ):
                paths[cid] = cur_path + (pkgs[cid]["name"],)
                queue.append(cid)
    return paths


def _pkg_requests_feature(
    pkg: dict[str, Any],
    node: dict[str, Any],
    target_pkg_name: str,
    feature_name: str,
) -> bool:
    """Return True if ``pkg`` actively requests ``feature_name`` on ``target_pkg_name``."""
    specs = [
        dep
        for dep in pkg.get("dependencies", [])
        if dep["name"] == target_pkg_name and dep.get("kind") in (None, "build")
    ]
    if not specs:
        return False

    activated_opt = _activated_optional_aliases(pkg, node)
    active_aliases: set[str] = set()
    for dep in specs:
        alias = dep.get("rename") or dep["name"]
        if not dep.get("optional", False) or alias in activated_opt:
            active_aliases.add(alias)
            if feature_name in dep.get("features", []):
                return True

    if not active_aliases:
        return False

    pkg_features = pkg.get("features", {})
    for feat in node.get("features", []):
        for item in pkg_features.get(feat, []):
            if "/" not in item:
                continue
            lhs, rhs = item.split("/", 1)
            lhs_clean = lhs[:-1] if lhs.endswith("?") else lhs
            if lhs_clean in active_aliases and rhs == feature_name:
                return True
    return False


def _external_package_requests_feature(
    metadata: dict[str, Any],
    subgraph_ids: set[str],
    target_pkg_name: str,
    feature_name: str,
) -> bool:
    """Return True if a non-subgraph package requests ``feature_name`` on ``target_pkg_name``."""
    pkgs = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    for nid, node in nodes.items():
        if nid in subgraph_ids:
            continue
        if _pkg_requests_feature(pkgs[nid], node, target_pkg_name, feature_name):
            return True
    return False


def check_shared_subgraph(
    metadata: dict[str, Any],
    root_name: str,
    scope: str,
    *,
    allow_external_single_core_unification: bool = False,
) -> list[BoundaryViolation]:
    """Validate a single shared-crate subgraph inside ``metadata``."""
    pkgs = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    matches = [p["id"] for p in metadata["packages"] if p["name"] == root_name]
    if not matches:
        return [BoundaryViolation(scope, f"missing crate {root_name!r}", (root_name,))]

    root_id = matches[0]
    paths = _walk_graph(metadata, root_id)
    subgraph_ids = set(paths)
    violations: list[BoundaryViolation] = []

    for nid, path in paths.items():
        pkg = pkgs[nid]
        node = nodes[nid]
        name = pkg["name"]
        if name.startswith(SHARED_FORBIDDEN_PREFIXES):
            violations.append(BoundaryViolation(scope, f"forbidden package {name!r}", path))

        for feat in node.get("features", []):
            if feat not in SHARED_FORBIDDEN_FEATURES:
                continue
            if feat != "unsafe-assume-single-core":
                violations.append(
                    BoundaryViolation(scope, f"forbidden feature {feat!r} on {name!r}", path)
                )
                continue

            # In root workspace or fw-c3 metadata, Cargo unifies features across
            # all workspace members onto a single portable-atomic node, so fw-c3's
            # ``unsafe-assume-single-core`` appears there even when no shared
            # crate requested it. Still reject if any shared-subgraph crate
            # requested it, if any non-portable-atomic crate enabled it, or if
            # no external workspace member requested it.
            if name != "portable-atomic":
                violations.append(
                    BoundaryViolation(scope, f"forbidden feature {feat!r} on {name!r}", path)
                )
                continue

            requested_inside = any(
                _pkg_requests_feature(pkgs[sid], nodes[sid], "portable-atomic", feat)
                for sid in subgraph_ids
            ) or any(
                feat in pkg.get("features", {}).get(f, [])
                for f in node.get("features", [])
                if f != feat
            )
            requested_outside = (
                allow_external_single_core_unification
                and _external_package_requests_feature(
                    metadata, subgraph_ids, "portable-atomic", feat
                )
            )
            if requested_inside or not requested_outside:
                violations.append(
                    BoundaryViolation(scope, f"forbidden feature {feat!r} on {name!r}", path)
                )

    return violations


def check_c3_graph(metadata: dict[str, Any], scope: str) -> list[BoundaryViolation]:
    """Validate ``fw-c3`` dependencies and its reachable shared-crate subgraphs."""
    pkgs = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    root_id = metadata["resolve"].get("root")
    if not root_id:
        matches = [p["id"] for p in metadata["packages"] if p["name"] == "fw-c3"]
        if not matches:
            return [BoundaryViolation(scope, "missing crate 'fw-c3'", ("fw-c3",))]
        root_id = matches[0]

    paths = _walk_graph(metadata, root_id)
    violations: list[BoundaryViolation] = []

    for nid, path in paths.items():
        name = pkgs[nid]["name"]
        if name in C3_FORBIDDEN_EXACT or name.startswith(C3_FORBIDDEN_PREFIXES):
            violations.append(BoundaryViolation(scope, f"forbidden package {name!r}", path))
        for feat in nodes[nid].get("features", []):
            if feat in C3_FORBIDDEN_FEATURES:
                violations.append(
                    BoundaryViolation(scope, f"forbidden feature {feat!r} on {name!r}", path)
                )

    reachable_names = {pkgs[nid]["name"] for nid in paths}
    for crate in SHARED_CRATES:
        if crate in reachable_names:
            violations.extend(
                check_shared_subgraph(
                    metadata,
                    crate,
                    f"{scope}::{crate}",
                    allow_external_single_core_unification=True,
                )
            )
    return violations


def check_s3_graph(
    metadata: dict[str, Any],
    scope: str,
    *,
    filtered_target: bool,
) -> list[BoundaryViolation]:
    """Validate ``fw-s3`` dependencies and its reachable shared-crate subgraphs."""
    pkgs = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    root_id = metadata["resolve"].get("root")
    if not root_id:
        matches = [p["id"] for p in metadata["packages"] if p["name"] == "fw-s3"]
        if not matches:
            return [BoundaryViolation(scope, "missing crate 'fw-s3'", ("fw-s3",))]
        root_id = matches[0]

    paths = _walk_graph(
        metadata,
        root_id,
        exclude_riscv32_target_edges=not filtered_target,
    )
    violations: list[BoundaryViolation] = []

    for nid, path in paths.items():
        name = pkgs[nid]["name"]
        if name in S3_FORBIDDEN_EXACT:
            violations.append(BoundaryViolation(scope, f"forbidden package {name!r}", path))
        for feat in nodes[nid].get("features", []):
            if feat in S3_FORBIDDEN_FEATURES:
                violations.append(
                    BoundaryViolation(scope, f"forbidden feature {feat!r} on {name!r}", path)
                )

    reachable_names = {pkgs[nid]["name"] for nid in paths}
    for crate in SHARED_CRATES:
        if crate in reachable_names:
            violations.extend(
                check_shared_subgraph(
                    metadata,
                    crate,
                    f"{scope}::{crate}",
                    allow_external_single_core_unification=False,
                )
            )
    return violations


def check_shared_tree_output(tree_output: str, scope: str) -> list[BoundaryViolation]:
    """Verify standalone ``cargo tree`` output for the shared crates on the host."""
    violations: list[BoundaryViolation] = []
    for raw_line in tree_output.splitlines():
        line = raw_line.strip()
        if not line or "[dev-dependencies]" in line:
            continue
        # Strip tree-drawing prefixes to inspect ``<pkg> v<ver> [<feats>]``.
        idx = 0
        while idx < len(line) and not line[idx].isalnum():
            idx += 1
        entry = line[idx:]
        if not entry:
            continue
        pkg_name = entry.split(" ", 1)[0]
        if pkg_name.startswith(SHARED_FORBIDDEN_PREFIXES):
            violations.append(
                BoundaryViolation(scope, f"forbidden package {pkg_name!r}", (pkg_name,))
            )
        if "[" in entry and "]" in entry:
            feat_blob = entry[entry.rfind("[") + 1 : entry.rfind("]")]
            feats = {f.strip() for f in feat_blob.split(",") if f.strip()}
            for bad in sorted(feats & SHARED_FORBIDDEN_FEATURES):
                violations.append(
                    BoundaryViolation(
                        scope,
                        f"forbidden feature {bad!r} on {pkg_name!r}",
                        (pkg_name,),
                    )
                )
    return violations


def detect_host_target() -> str:
    """Return the active host target triple from ``rustc -vV``."""
    out = subprocess.check_output(["rustc", "-vV"], cwd=REPO_ROOT, text=True)
    for line in out.splitlines():
        if line.startswith("host: "):
            return line.split(": ", 1)[1].strip()
    raise RuntimeError("failed to parse host target from `rustc -vV`")


def _run_json(cmd: list[str]) -> dict[str, Any]:
    out = subprocess.check_output(cmd, cwd=REPO_ROOT, text=True)
    return json.loads(out)


def run_repo_checks(*, root_only: bool, require_s3: bool) -> int:
    """Run live repository dependency-boundary checks and report results."""
    if root_only and require_s3:
        print("error: --root-only and --require-s3 are mutually exclusive", file=sys.stderr)
        return 2

    s3_available = EXPORT_ESP_PATH.is_file()
    if require_s3 and not s3_available:
        print(
            f"error: S3 toolchain export missing at {EXPORT_ESP_PATH}; run tools/setup-s3.sh",
            file=sys.stderr,
        )
        return 1

    host_target = detect_host_target()
    violations: list[BoundaryViolation] = []

    print(f"Checking root shared crates on {host_target}...")
    root_meta = _run_json(
        [
            "tools/cargo.sh",
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            host_target,
        ]
    )
    for crate in SHARED_CRATES:
        violations.extend(
            check_shared_subgraph(
                root_meta,
                crate,
                f"host({host_target})::{crate}",
                allow_external_single_core_unification=True,
            )
        )

    tree_cmd = [
        "tools/cargo.sh",
        "tree",
        "--locked",
        "--target",
        host_target,
        "-e",
        "normal,build",
        "-f",
        "{p} [{f}]",
    ]
    for crate in SHARED_CRATES:
        tree_cmd.extend(["-p", crate])
    tree_out = subprocess.check_output(tree_cmd, cwd=REPO_ROOT, text=True)
    violations.extend(check_shared_tree_output(tree_out, f"host-tree({host_target})"))

    print(f"Checking fw-c3 on {C3_TARGET} (default)...")
    c3_default_meta = _run_json(
        [
            "tools/cargo.sh",
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
            "fw-c3/Cargo.toml",
            "--filter-platform",
            C3_TARGET,
        ]
    )
    violations.extend(check_c3_graph(c3_default_meta, f"fw-c3({C3_TARGET},default)"))

    print(f"Checking fw-c3 on {C3_TARGET} (device-x3)...")
    c3_x3_meta = _run_json(
        [
            "tools/cargo.sh",
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
            "fw-c3/Cargo.toml",
            "--features",
            "device-x3",
            "--filter-platform",
            C3_TARGET,
        ]
    )
    violations.extend(check_c3_graph(c3_x3_meta, f"fw-c3({C3_TARGET},device-x3)"))

    print("Checking fw-s3 (unfiltered metadata)...")
    s3_unfiltered_meta = _run_json(
        [
            "tools/cargo.sh",
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
            "fw-s3/Cargo.toml",
        ]
    )
    violations.extend(
        check_s3_graph(s3_unfiltered_meta, "fw-s3(unfiltered)", filtered_target=False)
    )

    if root_only:
        print(f"S3 target-filtered check ({S3_TARGET}): omitted (--root-only mode).")
    elif not s3_available:
        print(
            f"S3 target-filtered check ({S3_TARGET}): omitted "
            f"({EXPORT_ESP_PATH.relative_to(REPO_ROOT)} not found)."
        )
    else:
        print(f"Checking fw-s3 on {S3_TARGET} via tools/cargo-s3.sh...")
        s3_filtered_meta = _run_json(
            [
                "tools/cargo-s3.sh",
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--filter-platform",
                S3_TARGET,
            ]
        )
        violations.extend(
            check_s3_graph(s3_filtered_meta, f"fw-s3({S3_TARGET})", filtered_target=True)
        )
        print(f"S3 target-filtered check ({S3_TARGET}): ran.")

    if violations:
        print(f"Found {len(violations)} dependency boundary violation(s):", file=sys.stderr)
        for v in violations:
            chain = " -> ".join(v.path)
            print(f"  [{v.scope}] {v.message} (path: {chain})", file=sys.stderr)
        return 1

    print("All dependency boundary checks passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--root-only",
        action="store_true",
        help="Check root shared crates, fw-c3, and unfiltered fw-s3 without S3 toolchain.",
    )
    mode.add_argument(
        "--require-s3",
        action="store_true",
        help="Fail if target/toolchains/export-esp.sh is missing; check S3 target-filtered graph.",
    )
    args = parser.parse_args(argv)
    return run_repo_checks(root_only=args.root_only, require_s3=args.require_s3)


def _pkg(
    name: str,
    *,
    features: dict[str, list[str]] | None = None,
    dependencies: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    return {
        "id": f"pkg:{name}",
        "name": name,
        "features": features or {},
        "dependencies": dependencies or [],
    }


def _node(
    name: str,
    *,
    features: list[str] | None = None,
    deps: list[tuple[str, list[dict[str, Any]]]] | None = None,
) -> dict[str, Any]:
    return {
        "id": f"pkg:{name}",
        "features": features or [],
        "deps": [
            {"name": dep_name, "pkg": f"pkg:{dep_name}", "dep_kinds": kinds}
            for dep_name, kinds in (deps or [])
        ],
    }


def _meta(
    packages: list[dict[str, Any]],
    nodes: list[dict[str, Any]],
    *,
    root: str | None = None,
) -> dict[str, Any]:
    return {
        "packages": packages,
        "resolve": {
            "root": f"pkg:{root}" if root else None,
            "nodes": nodes,
        },
    }


class DependencyBoundaryTests(unittest.TestCase):
    """Synthetic cargo-metadata boundary checks."""

    def test_clean_shared_c3_and_s3_graphs_pass(self) -> None:
        shared_meta = _meta(
            [
                _pkg("fw-common"),
                _pkg("app-core"),
                _pkg("portable-atomic"),
                _pkg("heapless"),
            ],
            [
                _node(
                    "fw-common",
                    deps=[
                        ("app-core", [{"kind": None, "target": None}]),
                        ("portable-atomic", [{"kind": None, "target": None}]),
                    ],
                ),
                _node("app-core", deps=[("heapless", [{"kind": None, "target": None}])]),
                _node("portable-atomic", features=["default", "fallback"]),
                _node("heapless"),
            ],
        )
        self.assertEqual(check_shared_subgraph(shared_meta, "fw-common", "shared"), [])

        # C3 is allowed to enable unsafe-assume-single-core on portable-atomic
        # and esp-hal/esp32c3 while leaving fw-common clean. Also test that an
        # inactive weak optional edge esp-hal -> esp32s3 does not false-positive.
        c3_meta = _meta(
            [
                _pkg(
                    "fw-c3",
                    dependencies=[
                        {
                            "name": "portable-atomic",
                            "kind": None,
                            "optional": False,
                            "features": ["unsafe-assume-single-core"],
                        },
                        {"name": "fw-common", "kind": None, "optional": False, "features": []},
                        {
                            "name": "esp-hal",
                            "kind": None,
                            "optional": False,
                            "features": ["esp32c3"],
                        },
                    ],
                ),
                _pkg(
                    "fw-common",
                    dependencies=[
                        {"name": "portable-atomic", "kind": None, "optional": False, "features": []}
                    ],
                ),
                _pkg("portable-atomic"),
                _pkg(
                    "esp-hal",
                    features={
                        "rt": ["esp32c3?/rt", "esp32s3?/rt"],
                        "esp32c3": ["dep:esp32c3", "portable-atomic/unsafe-assume-single-core"],
                        "esp32s3": ["dep:esp32s3"],
                    },
                    dependencies=[
                        {"name": "esp32c3", "kind": None, "optional": True, "features": []},
                        {"name": "esp32s3", "kind": None, "optional": True, "features": []},
                    ],
                ),
                _pkg("esp32c3"),
                _pkg("esp32s3"),
            ],
            [
                _node(
                    "fw-c3",
                    deps=[
                        ("fw-common", [{"kind": None, "target": None}]),
                        ("portable-atomic", [{"kind": None, "target": None}]),
                        ("esp-hal", [{"kind": None, "target": None}]),
                    ],
                ),
                _node("fw-common", deps=[("portable-atomic", [{"kind": None, "target": None}])]),
                _node("portable-atomic", features=["default", "unsafe-assume-single-core"]),
                _node(
                    "esp-hal",
                    features=["rt", "esp32c3"],
                    deps=[
                        ("esp32c3", [{"kind": None, "target": None}]),
                        ("esp32s3", [{"kind": None, "target": None}]),
                    ],
                ),
                _node("esp32c3"),
                _node("esp32s3"),
            ],
            root="fw-c3",
        )
        self.assertEqual(check_c3_graph(c3_meta, "fw-c3"), [])

        # S3 allows esp-bootloader-esp-idf with esp32s3 and forbids riscv32/c3.
        s3_meta = _meta(
            [
                _pkg("fw-s3"),
                _pkg("fw-common"),
                _pkg("portable-atomic"),
                _pkg("esp-hal"),
                _pkg("esp-bootloader-esp-idf"),
                _pkg("xtensa-lx-rt"),
            ],
            [
                _node(
                    "fw-s3",
                    features=["x4-pro"],
                    deps=[
                        ("fw-common", [{"kind": None, "target": None}]),
                        ("esp-hal", [{"kind": None, "target": None}]),
                        ("esp-bootloader-esp-idf", [{"kind": None, "target": None}]),
                    ],
                ),
                _node("fw-common", deps=[("portable-atomic", [{"kind": None, "target": None}])]),
                _node("portable-atomic", features=["default", "require-cas"]),
                _node(
                    "esp-hal",
                    features=["esp32s3"],
                    deps=[("xtensa-lx-rt", [{"kind": None, "target": None}])],
                ),
                _node("esp-bootloader-esp-idf", features=["esp32s3"]),
                _node("xtensa-lx-rt"),
            ],
            root="fw-s3",
        )
        self.assertEqual(check_s3_graph(s3_meta, "fw-s3", filtered_target=True), [])

    def test_catches_esp_packages_in_shared_normal_and_build_deps(self) -> None:
        normal_meta = _meta(
            [_pkg("storage"), _pkg("esp-hal")],
            [
                _node("storage", deps=[("esp-hal", [{"kind": None, "target": None}])]),
                _node("esp-hal"),
            ],
        )
        violations = check_shared_subgraph(normal_meta, "storage", "shared")
        self.assertTrue(any("esp-hal" in v.message for v in violations))

        build_meta = _meta(
            [_pkg("reader-cache"), _pkg("esp-alloc")],
            [
                _node("reader-cache", deps=[("esp-alloc", [{"kind": "build", "target": None}])]),
                _node("esp-alloc"),
            ],
        )
        violations = check_shared_subgraph(build_meta, "reader-cache", "shared")
        self.assertTrue(any("esp-alloc" in v.message for v in violations))

    def test_ignores_dev_dependencies(self) -> None:
        dev_meta = _meta(
            [_pkg("fw-common"), _pkg("critical-section"), _pkg("fatfs"), _pkg("esp-hal")],
            [
                _node(
                    "fw-common",
                    deps=[
                        ("critical-section", [{"kind": "dev", "target": None}]),
                        ("fatfs", [{"kind": "dev", "target": None}]),
                        ("esp-hal", [{"kind": "dev", "target": None}]),
                    ],
                ),
                _node("critical-section", features=["std"]),
                _node("fatfs"),
                _node("esp-hal", features=["esp32c3"]),
            ],
        )
        self.assertEqual(check_shared_subgraph(dev_meta, "fw-common", "shared"), [])

    def test_catches_unsafe_assume_single_core_in_fw_common_and_fw_s3(self) -> None:
        common_leak = _meta(
            [
                _pkg(
                    "fw-common",
                    dependencies=[
                        {
                            "name": "portable-atomic",
                            "kind": None,
                            "optional": False,
                            "features": ["unsafe-assume-single-core"],
                        }
                    ],
                ),
                _pkg("portable-atomic"),
            ],
            [
                _node("fw-common", deps=[("portable-atomic", [{"kind": None, "target": None}])]),
                _node("portable-atomic", features=["unsafe-assume-single-core"]),
            ],
        )
        violations = check_shared_subgraph(common_leak, "fw-common", "shared")
        self.assertTrue(any("unsafe-assume-single-core" in v.message for v in violations))

        # Even inside fw-c3 where fw-c3 enables unsafe-assume-single-core,
        # fw-common requesting it directly must still be caught.
        common_leak_in_c3 = _meta(
            [
                _pkg(
                    "fw-c3",
                    dependencies=[
                        {
                            "name": "portable-atomic",
                            "kind": None,
                            "optional": False,
                            "features": ["unsafe-assume-single-core"],
                        },
                        {"name": "fw-common", "kind": None, "optional": False, "features": []},
                    ],
                ),
                _pkg(
                    "fw-common",
                    dependencies=[
                        {
                            "name": "portable-atomic",
                            "kind": None,
                            "optional": False,
                            "features": ["unsafe-assume-single-core"],
                        }
                    ],
                ),
                _pkg("portable-atomic"),
            ],
            [
                _node(
                    "fw-c3",
                    deps=[
                        ("fw-common", [{"kind": None, "target": None}]),
                        ("portable-atomic", [{"kind": None, "target": None}]),
                    ],
                ),
                _node("fw-common", deps=[("portable-atomic", [{"kind": None, "target": None}])]),
                _node("portable-atomic", features=["unsafe-assume-single-core"]),
            ],
            root="fw-c3",
        )
        c3_violations = check_c3_graph(common_leak_in_c3, "fw-c3")
        self.assertTrue(any("unsafe-assume-single-core" in v.message for v in c3_violations))

        s3_leak = _meta(
            [_pkg("fw-s3"), _pkg("portable-atomic")],
            [
                _node("fw-s3", deps=[("portable-atomic", [{"kind": None, "target": None}])]),
                _node("portable-atomic", features=["unsafe-assume-single-core"]),
            ],
            root="fw-s3",
        )
        s3_violations = check_s3_graph(s3_leak, "fw-s3", filtered_target=True)
        self.assertTrue(any("unsafe-assume-single-core" in v.message for v in s3_violations))

    def test_catches_cross_chip_contamination(self) -> None:
        bad_c3 = _meta(
            [_pkg("fw-c3"), _pkg("esp32s3")],
            [
                _node("fw-c3", deps=[("esp32s3", [{"kind": None, "target": None}])]),
                _node("esp32s3", features=["esp32s3"]),
            ],
            root="fw-c3",
        )
        c3_violations = check_c3_graph(bad_c3, "fw-c3")
        self.assertTrue(any("esp32s3" in v.message for v in c3_violations))

        bad_s3 = _meta(
            [_pkg("fw-s3"), _pkg("esp32c3"), _pkg("esp-riscv-rt")],
            [
                _node(
                    "fw-s3",
                    deps=[
                        ("esp32c3", [{"kind": None, "target": None}]),
                        ("esp-riscv-rt", [{"kind": None, "target": None}]),
                    ],
                ),
                _node("esp32c3", features=["esp32c3"]),
                _node("esp-riscv-rt"),
            ],
            root="fw-s3",
        )
        s3_violations = check_s3_graph(bad_s3, "fw-s3", filtered_target=True)
        self.assertTrue(any("esp32c3" in v.message for v in s3_violations))
        self.assertTrue(any("esp-riscv-rt" in v.message for v in s3_violations))

    def test_require_s3_fails_when_export_script_missing(self) -> None:
        missing = Path("/nonexistent/target/toolchains/export-esp.sh")
        stderr = io.StringIO()
        with (
            mock.patch(__name__ + ".EXPORT_ESP_PATH", missing),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(run_repo_checks(root_only=False, require_s3=True), 1)
        self.assertIn("S3 toolchain export missing", stderr.getvalue())


if __name__ == "__main__":
    sys.exit(main())
