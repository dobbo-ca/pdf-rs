#!/bin/sh
# tests/parity/go/build.sh <binary>: builds the Go oracle. It copies oracle/, upstream/ and deps/
# to a temporary directory, applies patches/determinism.patch to the copy of upstream/ (docs/
# parity.md lists every hunk) and builds there, so the vendored upstream stays verbatim. The
# oracle needs the patched hooks (pdfcpu.TimeNow and pdfcpu.RandReader) and does not build
# against the plain snapshot.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
case $1 in
/*) out=$1 ;;
*) out=$(pwd)/$1 ;;
esac
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp -R "$here/upstream" "$here/deps" "$here/oracle" "$tmp/"
(cd "$tmp/upstream" && git apply -p1 "$here/patches/determinism.patch")
(cd "$tmp/oracle" && go build -o "$out" .)
