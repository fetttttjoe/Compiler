#!/usr/bin/env python3
"""Differential fuzzer (ADR 0012 law 6 / ADR 0018): random programs
through both engines; stdout and exit codes must agree byte-for-byte.

Usage: tools/fuzz.py [seed_start] [seed_end]   (default 0..200)

The generator covers ints, bools, floats, strings, the conversions
(int()/float()/string()), template literals, int arrays with for-in
and push, control flow, and int->int helper calls. Structs and optionals are NOT
generated yet - cover those manually (or extend the generator) when
touching their lowering. A program whose float path traps int() at
runtime is skipped like any other oracle-diagnosed run. A divergence
saves the program next to this script and exits nonzero.
"""

import random
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMPILER = ROOT / "target" / "debug" / "Compiler"

INT_BIN = ["+", "-", "*"]
FLOAT_BIN = ["+", "-", "*", "/", "%"]
CMP = ["==", "!=", "<", "<=", ">", ">="]


def of_type(vars_, ty):
    return [n for n, t in vars_ if t == ty]


class Gen:
    """vars_ is a lexically threaded pool of (name, type) pairs; inner
    blocks receive copies, so a generated name never escapes its scope."""

    def __init__(self, rng):
        self.r = rng
        self.tmp = 0

    def name(self, prefix):
        self.tmp += 1
        return f"{prefix}{self.tmp}"

    def int_expr(self, vars_, depth=0):
        r = self.r
        ints = of_type(vars_, "int")
        if depth > 3 or r.random() < 0.3:
            if ints and r.random() < 0.6:
                return r.choice(ints)
            return str(r.randint(-100, 100))
        a = self.int_expr(vars_, depth + 1)
        b = self.int_expr(vars_, depth + 1)
        op = r.choice(INT_BIN + ["/", "%"])
        if op in "/%":
            if ints and r.random() < 0.4:
                # A runtime divisor exercises the checked path (ADR
                # 0022). d*d + 1 is provably never 0 or -1, even under
                # wrapping (squares mod 8 are 0/1/4), so the oracle
                # still runs clean.
                d = r.choice(ints)
                b = f"({d} * {d} + 1)"
            else:
                # Nonzero constants strength-reduce (pow2/magic paths).
                b = str(r.choice([2, 3, 4, 7, 8, 10, 12, 100]))
        return f"({a} {op} {b})"

    def float_expr(self, vars_, depth=0):
        r = self.r
        floats = of_type(vars_, "float")
        if depth > 2 or r.random() < 0.35:
            if floats and r.random() < 0.6:
                return r.choice(floats)
            if r.random() < 0.3:
                return f"float({self.int_expr(vars_, 3)})"
            # repr of a bounded float is always dotted decimal - the
            # lexer has no exponent form.
            return repr(round(r.uniform(-100, 100), 3))
        a = self.float_expr(vars_, depth + 1)
        b = self.float_expr(vars_, depth + 1)
        # / and % may produce inf/NaN; both engines are IEEE and the
        # text formatter is diff-pinned, so printing them is fair game.
        return f"({a} {r.choice(FLOAT_BIN)} {b})"

    def str_expr(self, vars_):
        r = self.r
        strs = of_type(vars_, "str")
        if r.random() < 0.35:
            # Template literal (ADR 0030): text runs and ${} parts. The
            # generated sub-expressions contain no braces or backticks,
            # so no escaping is needed.
            bits = ["`"]
            for _ in range(r.randint(1, 3)):
                bits.append(r.choice(["t", "u ", "", "-"]))
                roll = r.random()
                if strs and roll < 0.25:
                    bits.append(f"${{{r.choice(strs)}}}")
                elif roll < 0.6:
                    bits.append(f"${{{self.int_expr(vars_, 3)}}}")
                elif roll < 0.8:
                    bits.append(f"${{{self.bool_expr(vars_)}}}")
                else:
                    bits.append(f"${{{self.float_expr(vars_)}}}")
            bits.append(r.choice(["", "end"]))
            bits.append("`")
            return "".join(bits)
        parts = []
        for _ in range(r.randint(1, 3)):
            roll = r.random()
            if strs and roll < 0.3:
                parts.append(r.choice(strs))
            elif roll < 0.5:
                parts.append(f'"{r.choice(["a", "b", "xy", ""])}"')
            elif roll < 0.7:
                parts.append(f"string({self.int_expr(vars_, 3)})")
            elif roll < 0.85:
                parts.append(f"string({self.bool_expr(vars_)})")
            else:
                parts.append(f"string({self.float_expr(vars_)})")
        return " + ".join(parts)

    def bool_expr(self, vars_):
        r = self.r
        if r.random() < 0.25:
            a = self.float_expr(vars_, 2)
            b = self.float_expr(vars_, 2)
        else:
            a = self.int_expr(vars_)
            b = self.int_expr(vars_)
        e = f"({a} {r.choice(CMP)} {b})"
        if r.random() < 0.3:
            c = self.int_expr(vars_)
            d = self.int_expr(vars_)
            joiner = r.choice(["&&", "||"])
            e = f"({e} {joiner} ({c} {r.choice(CMP)} {d}))"
        return e

    def assign(self, vars_):
        """One reassignment of a v*/g*/s* variable (loop counters i*
        stay untouched - reassigning one makes nonterminating loops)."""
        mut = [(n, t) for n, t in vars_ if not n.startswith(("i", "p"))]
        n, t = self.r.choice(mut)
        if t == "int":
            return f"{n} = {self.int_expr(vars_)};"
        if t == "float":
            return f"{n} = {self.float_expr(vars_)};"
        # Growth is bounded: loops run at most 4 iterations, two deep.
        return f"{n} = {n} + {self.str_expr(vars_)};"

    def body(self, vars_, depth=0):
        r = self.r
        lines = []
        for _ in range(r.randint(1, 5)):
            roll = r.random()
            if roll < 0.2:
                v = self.name("v")
                lines.append(f"var {v}: int = {self.int_expr(vars_)};")
                vars_ = vars_ + [(v, "int")]
            elif roll < 0.3:
                v = self.name("g")
                lines.append(f"var {v}: float = {self.float_expr(vars_)};")
                vars_ = vars_ + [(v, "float")]
            elif roll < 0.38:
                v = self.name("s")
                lines.append(f"var {v}: string = {self.str_expr(vars_)};")
                vars_ = vars_ + [(v, "str")]
            elif roll < 0.44:
                # int() of a bounded float: fmod keeps it in range, so
                # only an inf/NaN operand upstream can trap (-> skip).
                v = self.name("v")
                lines.append(f"var {v}: int = int({self.float_expr(vars_)} % 997.0);")
                vars_ = vars_ + [(v, "int")]
            elif roll < 0.52 and [n for n, t in vars_ if not n.startswith(("i", "p"))]:
                lines.append(self.assign(vars_))
            elif roll < 0.62:
                lines.append(f"print({self.int_expr(vars_)});")
            elif roll < 0.68:
                lines.append(f"print({self.float_expr(vars_)});")
            elif roll < 0.74:
                lines.append(f"print({self.str_expr(vars_)});")
            elif roll < 0.8:
                lines.append(f"print({self.bool_expr(vars_)});")
            elif roll < 0.9 and depth < 2:
                inner = self.body(list(vars_), depth + 1)
                lines.append(
                    f"if {self.bool_expr(vars_)} {{ {inner} }} else {{ {self.body(list(vars_), depth + 1)} }}"
                )
            elif depth < 2:
                v = self.name("i")
                inner = self.body(vars_ + [(v, "int")], depth + 1)
                # break only shortens a loop, so termination is safe;
                # generated `continue` could skip the counter and hang.
                guard = (
                    f"if {self.bool_expr(vars_ + [(v, 'int')])} {{ break; }} "
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
            pool = [(p, "int") for p in params]
            body = self.body(list(pool))
            ret = self.int_expr(list(pool))
            helpers.append(f"fun {fname}({sig}): int {{ {body} return {ret}; }}")
        main_body = self.body([])
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
