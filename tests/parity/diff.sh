#!/bin/sh
# tests/parity/diff.sh <go-out> <rust-out>: exits 1 if the trees differ. Each file is one
# (input, config) with sections `== <op> <kind>`. On a difference it prints:
#   - the number of differing files and files that only one side wrote;
#   - per section kind (the op name without its object number, e.g. `i png.out`, plus the config
#     letter), how many files differ in that section;
#   - for each section kind, up to four examples from different inputs: the file and the first
#     lines of that section's diff.
# Everything printed is capped at about 600 lines.
set -eu
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
if diff -rq "$1" "$2" >"$t/list"; then
  exit 0
fi
grep '^Only in' "$t/list" >"$t/only" || true
sed -n 's/^Files \([^ ]*\) and .*/\1/p' "$t/list" >"$t/files"

# One line per (file, differing section): "<kind> <config> <file> <section header>".
while IFS= read -r f; do
  b=$(basename "$f")
  c=$(echo "$b" | sed 's/.*\.\([a-z]\)\.txt$/\1/')
  awk -v file="$b" -v cfg="$c" '
    /^== / { sec = $0; next }
    FNR == NR { a[sec] = a[sec] "\n" $0; seen[sec] = 1; next }
    { r[sec] = r[sec] "\n" $0; seen[sec] = 1 }
    END {
      for (s in seen) if (a[s] != r[s]) {
        k = s; sub(/^== /, "", k); split(k, p, " "); op = p[1]; gsub(/[0-9]+/, "", op)
        print op " " p[2] " " cfg " " file " " s
      }
    }' "$f" "$2/$b"
done <"$t/files" >"$t/secs"

{
  echo "$(wc -l <"$t/files") files differ, $(wc -l <"$t/only") files on one side only"
  head -5 "$t/only"
  echo "-- differing sections by kind (op, kind, config):"
  cut -d' ' -f1-3 "$t/secs" | sort | uniq -c | sort -rn | head -60
  echo "-- examples:"
  cut -d' ' -f1-2 "$t/secs" | sort -u | head -40 | while IFS= read -r k; do
    # Up to four examples, each from a different input (the config letter stripped).
    grep "^$k " "$t/secs" | awk '{ s = $4; sub(/\.[a-z]\.txt$/, "", s) } !seen[s]++' | head -4 | while IFS= read -r line; do
      b=$(echo "$line" | cut -d' ' -f4)
      s=$(echo "$line" | cut -d' ' -f5-)
      echo "== [$k] $b :: $s"
      for side in "$1" "$2"; do
        awk -v s="$s" '/^== / { on = ($0 == s); next } on' "$side/$b" >"$t/$(basename "$side").sec"
      done
      diff "$t/$(basename "$1").sec" "$t/$(basename "$2").sec" | head -6 | cut -c1-240 || true
    done
  done
} | head -600
exit 1
