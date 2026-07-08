#!/usr/bin/env bash
# Benchmarks per ADR 0012 law 5: every .ys here pairs with an equivalent
# .c built at -O2. The runner first checks both binaries agree on the
# exit code (a correctness diff at benchmark scale), then compares
# best-of-3 wall times. Usage: benches/run.sh [name...]
set -eu
cd "$(dirname "$0")"

compiler=${COMPILER:-../target/debug/Compiler}
if [ ! -x "$compiler" ]; then
    (cd .. && cargo build -q)
fi
mkdir -p out

best_ms() {
    local best=-1 t0 t1 dt
    for _ in 1 2 3; do
        t0=$(date +%s%N)
        "$1" || true
        t1=$(date +%s%N)
        dt=$(((t1 - t0) / 1000000))
        if [ "$best" -lt 0 ] || [ "$dt" -lt "$best" ]; then best=$dt; fi
    done
    echo "$((best > 0 ? best : 1))"
}

names=("$@")
if [ ${#names[@]} -eq 0 ]; then names=(fib loop_sum primes collatz); fi

printf '%-10s %8s %8s %8s\n' bench ys-ms c-ms ratio
for name in "${names[@]}"; do
    name=${name%.ys}
    "$compiler" build "$name.ys" -o "out/${name}_ys"
    cc -O2 "$name.c" -o "out/${name}_c"
    set +e
    "out/${name}_ys" >/dev/null; ys_rc=$?
    "out/${name}_c" >/dev/null; c_rc=$?
    set -e
    if [ "$ys_rc" != "$c_rc" ]; then
        echo "MISMATCH $name: ys exit $ys_rc, c exit $c_rc" >&2
        exit 1
    fi
    ys_ms=$(best_ms "out/${name}_ys")
    c_ms=$(best_ms "out/${name}_c")
    printf '%-10s %8s %8s %8s\n' "$name" "$ys_ms" "$c_ms" \
        "$(awk "BEGIN{printf \"%.1fx\", $ys_ms / $c_ms}")"
done
