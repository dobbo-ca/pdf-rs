#!/bin/sh
# Time per class, Go and Rust on the same files, as two markdown tables: tests/parity/bench.sh [N].
# Needs `oracle` (Go), `bench` (Rust) and the inputs in /bench-in (made by `oracle -genbench`); run
# from the repo root. The classes and what each times are in docs/benchmarks.md.
#
#   1. Rust at its default max_threads against Go at its default.
#   2. Rust with max_threads 1 against Go with GOMAXPROCS=1.
#
# Each table has a row per class and a row per file. The script exits 1 when a class's ratio is
# over 1.10 (the bound is per class), when a byte count differs between Go and Rust, or when only
# one of them timed a file. A file over 1.10 is marked but does not fail the run by itself.
set -eu
export LC_ALL=C
n=${1:-5}
grep -m1 'model name' /proc/cpuinfo >&2 || true
d=/bench-in
tab=$(printf '\t')
# Best of three runs per file and side: the machine is shared, and one run can be slow.
best() {
  awk -F'\t' '!($1 in m) || $3 < m[$1] { m[$1] = $3; b[$1] = $2 } END { for (k in m) printf "%s\t%s\t%s\n", k, b[k], m[k] }' "$@" | sort
}
for i in 1 2 3; do
  oracle -bench "$n" $d >/tmp/go-def-$i.tsv
  GOMAXPROCS=1 oracle -bench "$n" $d >/tmp/go-one-$i.tsv
  bench $d "$n" >/tmp/rs-def-$i.tsv
  bench $d "$n" 1 >/tmp/rs-one-$i.tsv
done
best /tmp/go-def-?.tsv >/tmp/go-def.tsv
best /tmp/go-one-?.tsv >/tmp/go-one.tsv
best /tmp/rs-def-?.tsv >/tmp/rs-def.tsv
best /tmp/rs-one-?.tsv >/tmp/rs-one.tsv

# table TITLE GO.tsv RUST.tsv: name, Go bytes, Go ns, Rust bytes, Rust ns. ns per byte is ms per
# MB. Returns 1 when any class ratio is over 1.10, any byte count differs or a file is on one side
# only.
table() {
  bad=0
  echo "### $1"
  echo
  join -t "$tab" "$2" "$3" | tee /tmp/joined.tsv | awk -F'\t' '
function add(k) { b[k] += $2; g[k] += $3; r[k] += $5; c[k]++; if (!(k in seen)) { seen[k] = 1; order[++m] = k } }
{
  k = $1
  sub(/__.*/, "", k)
  add(k); add("all")
}
END {
  print "| class | files | MB | Go ms/MB | Rust ms/MB | Rust / Go | |"
  print "|---|---|---|---|---|---|---|"
  for (i = 1; i <= m; i++) {
    k = order[i]
    if (b[k] > 0 && g[k] > 0) {
      printf "| %s | %d | %.2f | %.3f | %.3f | %.2f | %s |\n", k, c[k], b[k] / 1e6, g[k] / b[k], r[k] / b[k], r[k] / g[k], (r[k] / g[k] > 1.10 ? "OVER 1.10" : "")
      if (r[k] / g[k] > 1.10) bad = 1
    }
  }
  exit bad
}' || bad=1
  # Per file, worst first: a ratio over 1.10 is marked, so a slow input is not hidden in its
  # class's row above. Also on stderr, for reading the log.
  echo
  echo "| file | MB | Go ms/MB | Rust ms/MB | Rust / Go | |"
  echo "|---|---|---|---|---|---|"
  awk -F'\t' '($2 > 0 || $4 > 0) && $3 > 0 { printf "%s\t%d\t%d\t%d\t%.2f\t%d\n", $1, $2, $3, $5, $5 / $3, $4 }' /tmp/joined.tsv | sort -t"$tab" -k5 -nr | tee /dev/stderr |
    awk -F'\t' '{
      mark = ""
      if ($5 > 1.10) mark = "OVER 1.10"
      if ($2 != $6) { mark = mark " BYTES DIFFER (Rust " $6 ")"; bad = 1 }
      d = $2 > 0 ? $2 : 1
      printf "| %s | %.2f | %.3f | %.3f | %.2f | %s |\n", $1, $2 / 1e6, $3 / d, $4 / d, $5, mark
    }
    END { exit bad }' || bad=1
  # A file that only one side timed (the other failed or skipped it) is a failure.
  cut -f1 "$2" | sort >/tmp/names-go.txt
  cut -f1 "$3" | sort >/tmp/names-rs.txt
  if ! comm -3 /tmp/names-go.txt /tmp/names-rs.txt >/tmp/names-diff.txt || [ -s /tmp/names-diff.txt ]; then
    echo
    echo "Timed by one side only (a name that starts with a tab: Rust only; the others: Go only):"
    echo
    sed 's/^/- /' /tmp/names-diff.txt
    bad=1
  fi
  echo
  return $bad
}

rc=0
table "Rust default max_threads against Go default" /tmp/go-def.tsv /tmp/rs-def.tsv || rc=1
table "Rust max_threads 1 against Go GOMAXPROCS=1" /tmp/go-one.tsv /tmp/rs-one.tsv || rc=1
exit $rc
