#!/usr/bin/env python3
"""Differential fuzzer (ADR 0012 law 6 / ADR 0018): random programs
through both engines; stdout and exit codes must agree byte-for-byte.

Usage: tools/fuzz.py [seed_start] [seed_end]   (default 0..200)

The generator covers ints, bools, floats, strings, the conversions
(int()/float()/string()), template literals, int arrays with for-in
and push, control flow, int->int helper calls, int? optionals (null
tests, ??, guard narrowing incl. the ADR 0033 guard-return-on-locals
shape), value structs (literals, field reads/writes, copy
semantics, structural equality, aggregate printing), and error unions
(ADR 0034: int! helpers, try chains, == error narrowing on both
branches, whole-union printing; ADR 0037: int! struct fields with
place-path narrowing and mutation, int![] arrays iterated and
printed, int! parameters), and generics (ADR 0035: fixed
templates instantiated at int/float/string/struct call sites — word,
XMM, and multi-word/sret shapes — optional wrapping through T? slots,
generic struct literals, field writes, and instance printing).
Payload enums (ADR 0036) get the same treatment:
a fixed monomorphic enum and a generic one, constructed at random
variants and consumed by match arms with payload bindings, `_`, and
`else`, plus enum equality. Refstructs and files are NOT
generated yet - cover those manually (or extend the generator) when
touching their lowering. A program whose
float path traps int() at runtime is skipped like any other
oracle-diagnosed run. A divergence saves the program next to this
script and exits nonzero.
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
            opts = of_type(vars_, "opt")
            pts = of_type(vars_, "pt")
            roll = r.random()
            # `o ?? k` and `q.x` are always-sound int atoms.
            if opts and roll < 0.15:
                return f"({r.choice(opts)} ?? {r.randint(-9, 9)})"
            if pts and roll < 0.3:
                return f"{r.choice(pts)}.{r.choice(['x', 'y'])}"
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
                    bits.append(f"${{{self.bool_expr(vars_, null_ok=True)}}}")
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

    def bool_expr(self, vars_, null_ok=False):
        """null_ok gates `o == null` atoms: legal only where no branch
        follows (print, templates) — as an if/while condition the
        checker narrows the tested var inside the branch, retyping it
        under the generator's feet (the guard arms model that; this
        does not)."""
        r = self.r
        opts = of_type(vars_, "opt")
        pts = of_type(vars_, "pt")
        if null_ok and opts and r.random() < 0.15:
            return f"({r.choice(opts)} {r.choice(['==', '!='])} null)"
        if len(pts) >= 2 and r.random() < 0.1:
            # Structural equality (ADR 0026), including through copies.
            a, b = r.sample(pts, 2)
            return f"({a} {r.choice(['==', '!='])} {b})"
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
        """One reassignment of a v*/g*/s*/o*/q* variable (loop counters
        i* stay untouched - reassigning one makes nonterminating loops;
        p* params and n* narrowed optionals are read-only: rebinding a
        narrowed local would kill its fact and unsound the generator)."""
        mut = [(n, t) for n, t in vars_ if not n.startswith(("i", "p", "n"))]
        n, t = self.r.choice(mut)
        if t == "int":
            return f"{n} = {self.int_expr(vars_)};"
        if t == "float":
            return f"{n} = {self.float_expr(vars_)};"
        if t == "opt":
            # Wrap points (ADR 0021): null and T both flow into T? slots.
            if self.r.random() < 0.3:
                return f"{n} = null;"
            return f"{n} = {self.int_expr(vars_)};"
        if t == "pt":
            if self.r.random() < 0.5:
                return f"{n}.{self.r.choice(['x', 'y'])} = {self.int_expr(vars_)};"
            return f"{n} = Pt {{ x: {self.int_expr(vars_)}, y: {self.int_expr(vars_)} }};"
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
            elif roll < 0.49:
                v = self.name("o")
                init = "null" if r.random() < 0.35 else self.int_expr(vars_)
                lines.append(f"var {v}: int? = {init};")
                vars_ = vars_ + [(v, "opt")]
            elif roll < 0.54:
                v = self.name("q")
                lines.append(
                    f"var {v}: Pt = Pt {{ x: {self.int_expr(vars_)}, y: {self.int_expr(vars_)} }};"
                )
                vars_ = vars_ + [(v, "pt")]
            elif roll < 0.58 and of_type(vars_, "pt"):
                # Copy semantics observable (ADR 0006): mutate the copy,
                # print both.
                src = r.choice(of_type(vars_, "pt"))
                v = self.name("q")
                lines.append(
                    f"var {v}: Pt = {src}; {v}.x = {self.int_expr(vars_)}; "
                    f"print({src}.x); print({v}.x);"
                )
                vars_ = vars_ + [(v, "pt")]
            elif roll < 0.62 and [n for n, t in vars_ if not n.startswith(("i", "p", "n"))]:
                lines.append(self.assign(vars_))
            elif roll < 0.68:
                lines.append(f"print({self.int_expr(vars_)});")
            elif roll < 0.71:
                lines.append(f"print({self.float_expr(vars_)});")
            elif roll < 0.74:
                lines.append(f"print({self.str_expr(vars_)});")
            elif roll < 0.77:
                lines.append(f"print({self.bool_expr(vars_, null_ok=True)});")
            elif roll < 0.81 and (of_type(vars_, "opt") or of_type(vars_, "pt")):
                # Tag-dispatch and aggregate printing.
                pool = of_type(vars_, "opt") + of_type(vars_, "pt")
                lines.append(f"print({r.choice(pool)});")
            elif roll < 0.86 and of_type(vars_, "opt") and depth < 2:
                # Guard narrowing (ADR 0007/0020): inside the branch the
                # optional reads as int, so it leaves the inner pool —
                # `o ?? k` / `o == null` on a narrowed int are type
                # errors, and a reassignment would kill the fact under
                # later reads. The fresh int copy carries its value.
                o = r.choice(of_type(vars_, "opt"))
                v = self.name("v")
                inner_pool = [(n, t) for n, t in vars_ if n != o] + [(v, "int")]
                inner = self.body(inner_pool, depth + 1)
                lines.append(f"if {o} != null {{ var {v}: int = {o} + 1; {inner} }}")
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
            prologue = ""
            if r.random() < 0.4:
                # The ADR 0033 shape: guard-return narrows a LOCAL; the
                # narrowed name threads read-only (prefix n, see assign).
                o = self.name("n")
                init = "null" if r.random() < 0.3 else str(r.randint(-9, 9))
                prologue = (
                    f"var {o}: int? = {init}; "
                    f"if {o} == null {{ return {self.int_expr(list(pool))}; }} "
                )
                pool = pool + [(o, "int")]
            body = self.body(list(pool))
            ret = self.int_expr(list(pool))
            helpers.append(f"fun {fname}({sig}): int {{ {prologue}{body} return {ret}; }}")
        main_body = self.body([])
        calls = " ".join(
            f"print({n}({', '.join(str(r.randint(-9, 9)) for _ in range(arity))}));"
            for n, arity in names
        )
        # Error unions (ADR 0034): an int! helper that may err, a try
        # chain through a second helper, and a main-side narrowing test
        # of both branches. Self-contained — e* names never enter the
        # general pool, so no narrowing state needs modeling.
        err_part = ""
        if r.random() < 0.7:
            k = r.randint(-3, 3)
            a, b = r.randint(-6, 6), r.randint(-6, 6)
            err_part = (
                f"var e1: int! = tryboth({a}, {b}); "
                f"print(e1); "
                f"if e1 == error {{ print(e1 == error.Efuzz); }} "
                f"else {{ print(e1 + 1); }} "
            )
            helpers.append(
                "fun mayerr(n: int): int! { "
                f"if n < {k} {{ return error.Efuzz; }} "
                f"if n > 4 {{ return error.Egro; }} "
                "return n * 3; } "
                "fun tryboth(a: int, b: int): int! { "
                "const x: int = try mayerr(a); "
                "const y: int = try mayerr(b); "
                "return x + y; }"
            )
            # ADR 0037: unions in fields, elements, and params — one
            # struct with a T! field (narrow, mutate), one int![] walk
            # through a T!-param helper. Still self-contained.
            if r.random() < 0.6:
                v1 = "error.Efuzz" if r.random() < 0.4 else str(r.randint(-9, 9))
                v2 = "error.Egro" if r.random() < 0.4 else str(r.randint(-9, 9))
                err_part += (
                    f"var es: EBox = EBox {{ r: {v1} }}; "
                    "print(es); "
                    "if es.r != error { print(es.r * 2); } "
                    f"es.r = {v2}; "
                    "if es.r == error { print(es.r); } "
                    f"var ea: int![] = [{v1}, {v2}, mayerr({r.randint(-6, 6)})]; "
                    "print(ea); "
                    "for ex in ea { print(epick(ex)); } "
                )
                helpers.append(
                    "struct EBox { r: int! } "
                    "fun epick(x: int!): int { "
                    "if x == error { return -1; } "
                    "return x; }"
                )
        # Generics (ADR 0035): fixed self-contained templates — g* names
        # never enter the general pool — instantiated at randomized call
        # sites. Uninstantiated templates cost nothing, so the helpers
        # and GBox always declare; only the calls are rolled. Shapes:
        # word T (int), XMM T (float), multi-word T (string, Pt — sret
        # returns), T? wrapping, explicit vs inferred arguments.
        g_part = ""
        if r.random() < 0.7:
            gcalls = []
            for _ in range(r.randint(2, 5)):
                pick = r.random()
                if pick < 0.2:
                    gcalls.append(
                        f"print(gmax({self.int_expr([])}, {self.int_expr([])}));"
                    )
                elif pick < 0.35:
                    gcalls.append(
                        f"print(gmax({self.float_expr([])}, {self.float_expr([])}));"
                    )
                elif pick < 0.5:
                    gcalls.append(f"print(gid({self.str_expr([])}));")
                elif pick < 0.6:
                    gcalls.append(
                        f"print(gid(Pt {{ x: {self.int_expr([])}, "
                        f"y: {self.int_expr([])} }}));"
                    )
                elif pick < 0.7:
                    gcalls.append(
                        f"print(gid(gmax({self.int_expr([])}, {self.int_expr([])})));"
                    )
                elif pick < 0.85:
                    v = "null" if r.random() < 0.4 else self.int_expr([])
                    args = "<int>" if r.random() < 0.5 else ""
                    gcalls.append(f"print(gor{args}({v}, {self.int_expr([])}));")
                else:
                    gcalls.append(
                        f"print(GBox<float> {{ v: {self.float_expr([])} }});"
                    )
            gb = self.name("gb")
            gcalls.append(
                f"var {gb}: GBox<int>= GBox<int> {{ v: {self.int_expr([])} }}; "
                f"{gb}.v = {gb}.v + {self.int_expr([])}; "
                f"print({gb}); print({gb}.v);"
            )
            # Enums (ADR 0036): construct a random variant, match on it
            # (payload bindings, `_`, `else`), compare constructions.
            ge = self.name("ge")
            ctor = r.choice(
                [
                    f"GSt.GA({self.int_expr([])})",
                    f"GSt.GB({self.float_expr([])}, {self.str_expr([])})",
                    "GSt.GC()",
                ]
            )
            arms = (
                "GA(a) { print(a + 1); } "
                + r.choice(["GB(f, s) { print(s); print(f); } ", "GB(_, s) { print(s); } "])
                + r.choice(["GC { print(0); } ", "else { print(0); } "])
            )
            gcalls.append(
                f"var {ge}: GSt = {ctor}; print({ge}); "
                f"match {ge} {{ {arms}}} "
                f"print({ge} == {ctor}); "
                f"print({ge} == GSt.GA({r.randint(-9, 9)}));"
            )
            if r.random() < 0.6:
                ok = r.choice(["true", "false"])
                gcalls.append(
                    f"print(gwrap({self.int_expr([])}, {ok})); "
                    f"match gwrap({self.str_expr([])}, {ok}) "
                    f"{{ GOk(v) {{ print(v); }} else {{ print(\"none\"); }} }}"
                )
            g_part = " ".join(gcalls) + " "
        arr = self.name("xs")
        stop = f"if ix == {r.randint(3, 5)} {{ break; }} " if r.random() < 0.5 else ""
        arr_part = (
            f"var {arr}: int[] = [{', '.join(str(r.randint(-5, 5)) for _ in range(r.randint(1, 4)))}];"
            f" for [ix, x] in {arr} {{ print(ix * 100 + x); {stop}if len({arr}) < 6 {{ push({arr}, x + 1); }} }}"
        )
        ret = self.int_expr([])
        return "\n".join(
            [
                "struct Pt { x: int, y: int }",
                "struct GBox<T> { v: T }",
                "enum GSt { GA(int), GB(float, string), GC }",
                "enum GRes<T> { GOk(T), GNone }",
                "error Efuzz, Egro;",
                "fun gmax<T>(a: T, b: T): T { if a > b { return a; } return b; }",
                "fun gid<T>(x: T): T { return x; }",
                "fun gor<T>(v: T?, f: T): T { if v != null { return v; } return f; }",
                "fun gwrap<T>(x: T, ok: bool): GRes<T> "
                "{ if ok { return GRes<T>.GOk(x); } return GRes<T>.GNone(); }",
            ]
            + helpers
            + [
                f"fun main(): int {{ {main_body} {calls} {err_part}{g_part}{arr_part} "
                f"return ({ret}) % 251; }}"
            ]
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
