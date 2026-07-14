#!/usr/bin/env python3
"""Differential fuzzer (ADR 0012 law 6 / ADR 0018): random programs
through both engines; stdout and exit codes must agree byte-for-byte.

Usage: tools/fuzz.py [seed_start] [seed_end]   (default 0..200)

The generator currently covers ints, bools, int arrays with for-in and
push, control flow, and int->int helper calls. Structs, strings, floats,
and optionals are NOT generated yet — cover those manually (or extend
the generator) when touching their lowering. A divergence saves the
program next to this script and exits nonzero.
"""

import random
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMPILER = ROOT / "target" / "debug" / "Compiler"

INT_BIN = ["+", "-", "*"]
CMP = ["==", "!=", "<", "<=", ">", ">="]


class Gen:
    def __init__(self, rng):
        self.r = rng
        self.tmp = 0

    def name(self, prefix):
        self.tmp += 1
        return f"{prefix}{self.tmp}"

    def int_expr(self, vars_, depth=0):
        r = self.r
        if depth > 3 or r.random() < 0.3:
            if vars_ and r.random() < 0.6:
                return r.choice(vars_)
            return str(r.randint(-100, 100))
        a = self.int_expr(vars_, depth + 1)
        b = self.int_expr(vars_, depth + 1)
        op = r.choice(INT_BIN + ["/", "%"])
        if op in "/%":
            # Keep divisors nonzero constants so the oracle runs clean.
            b = str(r.choice([2, 3, 4, 7, 8, 10, 12, 100]))
        return f"({a} {op} {b})"

    def bool_expr(self, vars_):
        a = self.int_expr(vars_)
        b = self.int_expr(vars_)
        e = f"({a} {self.r.choice(CMP)} {b})"
        if self.r.random() < 0.3:
            c = self.int_expr(vars_)
            d = self.int_expr(vars_)
            joiner = self.r.choice(["&&", "||"])
            e = f"({e} {joiner} ({c} {self.r.choice(CMP)} {d}))"
        return e

    def body(self, vars_, depth=0):
        r = self.r
        lines = []
        for _ in range(r.randint(1, 5)):
            roll = r.random()
            if roll < 0.35:
                v = self.name("v")
                lines.append(f"var {v}: int = {self.int_expr(vars_)};")
                vars_ = vars_ + [v]
            elif roll < 0.5 and [v for v in vars_ if v.startswith("v")]:
                # Loop counters (i*) stay untouched — reassigning one
                # makes nonterminating programs.
                v = r.choice([v for v in vars_ if v.startswith("v")])
                lines.append(f"{v} = {self.int_expr(vars_)};")
            elif roll < 0.65:
                lines.append(f"print({self.int_expr(vars_)});")
            elif roll < 0.75:
                lines.append(f"print({self.bool_expr(vars_)});")
            elif roll < 0.85 and depth < 2:
                inner = self.body(list(vars_), depth + 1)
                lines.append(
                    f"if {self.bool_expr(vars_)} {{ {inner} }} else {{ {self.body(list(vars_), depth + 1)} }}"
                )
            elif depth < 2:
                v = self.name("i")
                inner = self.body(vars_ + [v], depth + 1)
                # break only shortens a loop, so termination is safe;
                # generated `continue` could skip the counter and hang.
                guard = (
                    f"if {self.bool_expr(vars_ + [v])} {{ break; }} "
                    if r.random() < 0.3
                    else ""
                )
                lines.append(
                    f"var {v}: int = 0; while {v} < {r.randint(1, 4)} {{ {guard}{inner} {v} = {v} + 1; }}"
                )
        return " ".join(lines)

    def program(self):
        r = self.r
        helpers = []
        names = []  # (name, arity)
        for _ in range(r.randint(0, 2)):
            fname = self.name("f")
            params = [self.name("p") for _ in range(r.randint(1, 3))]
            names.append((fname, len(params)))
            sig = ", ".join(f"{p}: int" for p in params)
            body = self.body(list(params))
            ret = self.int_expr(list(params))
            helpers.append(f"fun {fname}({sig}): int {{ {body} return {ret}; }}")
        main_vars = []
        main_body = self.body(main_vars)
        calls = " ".join(
            f"print({n}({', '.join(str(r.randint(-9, 9)) for _ in range(arity))}));"
            for n, arity in names
        )
        arr = self.name("xs")
        stop = f"if ix == {r.randint(3, 5)} {{ break; }} " if r.random() < 0.5 else ""
        arr_part = (
            f"var {arr}: int[] = [{', '.join(str(r.randint(-5, 5)) for _ in range(r.randint(1, 4)))}];"
            f" for [ix, x] in {arr} {{ print(ix * 100 + x); {stop}if len({arr}) < 6 {{ push({arr}, x + 1); }} }}"
        )
        ret = self.int_expr([])
        return "\n".join(
            helpers
            + [f"fun main(): int {{ {main_body} {calls} {arr_part} return ({ret}) % 251; }}"]
        )


def run_one(seed, workdir):
    src = workdir / f"fuzz_{seed}.ys"
    src.write_text(Gen(random.Random(seed)).program())
    try:
        oracle = subprocess.run(
            [COMPILER, src], capture_output=True, text=True, timeout=20
        )
    except subprocess.TimeoutExpired:
        return "skip"
    if oracle.returncode != 0:
        return "skip"  # generator hit a diagnostic; not a divergence
    stdout = oracle.stdout
    if not stdout.endswith("\n"):
        return "skip"
    cut = stdout.rfind("\n", 0, len(stdout) - 1)
    last = stdout[cut + 1 : -1]
    if not (last.startswith("=> Int(") and last.endswith(")")):
        return "skip"
    value = int(last[7:-1])
    prints = stdout[: cut + 1]

    binary = workdir / f"fuzz_{seed}"
    build = subprocess.run(
        [COMPILER, "build", src, "-o", binary], capture_output=True, text=True
    )
    if build.returncode != 0:
        return f"BUILDFAIL: {build.stderr}"
    ran = subprocess.run([binary], capture_output=True, text=True, timeout=20)
    if ran.returncode != value & 0xFF or ran.stdout != prints:
        keep = Path(__file__).parent / f"divergence_{seed}.ys"
        keep.write_text(src.read_text())
        return (
            f"DIVERGENCE seed={seed} (saved {keep}): "
            f"exit {ran.returncode} vs {value & 0xFF}, stdout match={ran.stdout == prints}"
        )
    return "ok"


def main():
    start = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    end = int(sys.argv[2]) if len(sys.argv) > 2 else 200
    subprocess.run(["cargo", "build", "-q"], cwd=ROOT, check=True)
    counts = {}
    with tempfile.TemporaryDirectory(prefix="ys-fuzz-") as tmp:
        for seed in range(start, end):
            result = run_one(seed, Path(tmp))
            counts[result.split(":")[0].split(" ")[0]] = (
                counts.get(result.split(":")[0].split(" ")[0], 0) + 1
            )
            if result.startswith(("DIVERGENCE", "BUILDFAIL")):
                print(result)
                sys.exit(1)
    print(counts)


if __name__ == "__main__":
    main()
