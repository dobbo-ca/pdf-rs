# CI image: every check runs inside this BuildKit build, so the runner needs only Docker. The
# aarch64 stages (arm64, parity-arm64, fuzz on arm64) build natively on ubuntu-24.04-arm.
# The crate depends on two dobbo-ca ports (deflate-rs, jpeg-rs) by git rev. They are fetched once
# in the `deps` stage; every later step runs offline.
# Every tool is installed in a stage before `COPY . .`, so a new commit does not install it again,
# and the fetch runs from the manifests and lockfiles alone, so a commit that leaves them alone
# does not fetch again.
FROM rust:1.99.1-bookworm AS toolchain
RUN rustup component add rustfmt clippy && rustup target add aarch64-unknown-linux-gnu
WORKDIR /src

# Only the manifests, with an empty target for what they name, so cargo can resolve them: the
# crate's, and the fuzz crate's (tests/fuzz, its own lockfile, a stub for every [[bin]] path).
FROM toolchain AS deps
COPY Cargo.toml Cargo.lock ./
COPY tests/fuzz/Cargo.toml tests/fuzz/Cargo.lock tests/fuzz/
RUN mkdir -p src tests/fuzz/fuzz_targets && touch src/lib.rs \
    && grep -o 'fuzz_targets/[a-z_0-9]*\.rs' tests/fuzz/Cargo.toml | sed 's|^|tests/fuzz/|' | xargs touch
RUN CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked --manifest-path tests/fuzz/Cargo.toml \
    && rm -rf src tests

FROM toolchain AS src
COPY --from=deps /usr/local/cargo/registry /usr/local/cargo/registry
COPY --from=deps /usr/local/cargo/git /usr/local/cargo/git
ENV CARGO_NET_OFFLINE=true
COPY . .

# The checks run in the build, so a red check fails the build; a rerun of the same source is a
# BuildKit cache hit. The target dir is a cache mount, so it persists across builds. No SIMD here,
# so there is no scalar-tier run and no NEON module to lint. `cargo test` includes the doc-tests.
FROM src AS ci
# The target cache is shared by every branch and cargo compares mtimes: another branch's newer
# build would be reused as this one's, so every cargo step touches the sources first.
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    cargo fmt --check
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo clippy --locked --all-targets -- -D warnings
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo test --locked
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo build --locked --release --target aarch64-unknown-linux-gnu

# aarch64 tests, native on ubuntu-24.04-arm. The crate has no SIMD of its own; what is
# aarch64-specific is the NEON code of deflate-rs (adler32) and jpeg-rs, reached through the
# filter and image tests, run for both tiers of those crates (the second run sets
# DEFLATE_SIMD=scalar JPEG_SIMD=scalar, which deflate-rs and jpeg-rs read; pdf-rs itself has no
# SIMD tier, and Go's image/jpeg has none).
FROM src AS arm64
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo test --locked
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && DEFLATE_SIMD=scalar JPEG_SIMD=scalar cargo test --locked --lib

# Go parity: the Go oracle (tests/parity/go, vendored, no token) writes the inputs and runs every
# operation; the Rust example runs the same inputs; the two output trees must be equal. The Go side
# is vendored under tests/parity/go: pdf-go at 113d693 (upstream) and the public modules it builds
# with (deps), all through `replace` directives, so the build needs no network and no token.
# build.sh applies tests/parity/go/patches/determinism.patch (sorted map loops, a clock hook and a
# random hook) to a copy of upstream, which stays verbatim in the repository.
FROM golang:1.27-bookworm AS go-oracle
ENV GOTOOLCHAIN=local CGO_ENABLED=0 GOFLAGS=-mod=mod GOPROXY=off
WORKDIR /src
COPY tests/parity/go tests/parity/go
RUN --mount=type=cache,id=pdf-go-build,target=/root/.cache/go-build sh tests/parity/go/build.sh /out/oracle

FROM go-oracle AS go-run
ENV GOMEMLIMIT=3GiB
COPY tests/data tests/data
COPY tests/parity/diff.sh tests/parity/diff.sh
RUN /out/oracle -gen tests/data /inputs && ls /inputs | wc -l && du -sh /inputs
# One file per (input, config), so the tree is about inputs x configs files.
RUN /out/oracle /inputs /go-out && du -sh /go-out && ls /go-out | wc -l
# The run-twice check (docs/design.md section 10): Go seeds every map's iteration order per
# process, so a second run walks the maps in other orders. Any difference is a map loop that the
# determinism patch misses.
RUN /out/oracle /inputs /go-out-2 && sh tests/parity/diff.sh /go-out /go-out-2 && rm -rf /go-out-2
# The subset the aarch64 oracle runs (stage parity-arm64): every 16th input and its
# `.limits` sidecar. Go runs it again, so the two trees have the same names.
RUN mkdir /sub && ls /inputs | grep -v '\.limits$' | awk 'NR % 16 == 0' | while IFS= read -r f; do cp "/inputs/$f" /sub/; if [ -f "/inputs/$f.limits" ]; then cp "/inputs/$f.limits" /sub/; fi; done \
    && /out/oracle /sub /go-sub && ls /sub | wc -l && ls /go-sub | wc -l

# The Go trees are bind mounts from go-run, not COPY, so a commit does not copy them again after
# `COPY . .`. The Rust oracle runs twice, with `-threads 1` and with the default max_threads, and
# both outputs must equal the Go tree. Each Rust output is deleted after its diff, pass or fail
# (the diff status is kept, then rm, then exit), so the layer and the cache export stay small.
# One file per (input, config): at least 3 configs per input, so the floor is 2 x inputs.
FROM src AS parity
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    --mount=type=bind,from=go-run,source=/inputs,target=/inputs,rw \
    --mount=type=bind,from=go-run,source=/go-out,target=/go-out,rw \
    echo "$(ls /inputs | wc -l) inputs, $(ls /go-out | wc -l) outputs" \
    && [ "$(ls /go-out | wc -l)" -ge "$(( $(ls /inputs | wc -l) * 2 ))" ] \
    && find src examples tests -name "*.rs" -exec touch {} + && cargo run --locked --release --example oracle -- -threads 1 /inputs /rust-out-1 \
    && { sh tests/parity/diff.sh /go-out /rust-out-1; s=$?; rm -rf /rust-out-1; [ $s -eq 0 ] || exit $s; } \
    && find src examples tests -name "*.rs" -exec touch {} + && cargo run --locked --release --example oracle -- /inputs /rust-out \
    && { sh tests/parity/diff.sh /go-out /rust-out; s=$?; rm -rf /rust-out; exit $s; }

# aarch64 against Go, native on ubuntu-24.04-arm: the oracle example on the subset, once per tier
# of the crates (the NEON code of deflate-rs and jpeg-rs, then DEFLATE_SIMD=scalar
# JPEG_SIMD=scalar, read by those crates, not by pdf-rs; Go has no SIMD); the output must equal
# Go's. Settles the amd64/arm64 shift mask question of docs/design.md.
FROM src AS parity-arm64
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    --mount=type=bind,from=go-run,source=/sub,target=/sub,rw \
    --mount=type=bind,from=go-run,source=/go-sub,target=/go-sub,rw \
    echo "$(ls /sub | wc -l) inputs, $(ls /go-sub | wc -l) outputs" \
    && [ "$(ls /go-sub | wc -l)" -ge "$(( $(ls /sub | wc -l) * 2 ))" ] \
    && find src examples tests -name "*.rs" -exec touch {} + && cargo run --locked --release --example oracle -- -threads 1 /sub /arm-out \
    && { sh tests/parity/diff.sh /go-sub /arm-out; s=$?; rm -rf /arm-out; [ $s -eq 0 ] || exit $s; } \
    && find src examples tests -name "*.rs" -exec touch {} + && DEFLATE_SIMD=scalar JPEG_SIMD=scalar cargo run --locked --release --example oracle -- /sub /arm-out-scalar \
    && { sh tests/parity/diff.sh /go-sub /arm-out-scalar; s=$?; rm -rf /arm-out-scalar; exit $s; }


# The seeds of the document targets: the small test PDFs (under 300 KB) and a sample of the
# oracle's `-gen` outputs (the mutations and the synthesized inputs), flat in /seeds/pdf. The
# `-gen` run is the same as go-run's, so the inputs are the same files.
FROM go-oracle AS go-gen
ENV GOMEMLIMIT=3GiB
COPY tests/data tests/data
RUN /out/oracle -gen tests/data /gen-in

FROM toolchain AS fuzz-seeds
COPY tests/data tests/data
RUN mkdir -p /seeds/pdf \
    && find tests/data -name '*.pdf' -size -300k | sort | while IFS= read -r f; do cp "$f" "/seeds/pdf/$(echo "$f" | tr '/ ' '__')"; done
RUN --mount=type=bind,from=go-gen,source=/gen-in,target=/gen-in,rw \
    find /gen-in -name '*.pdf' -size -64k | sort | awk 'NR % 25 == 0' | head -400 | while IFS= read -r f; do cp "$f" "/seeds/pdf/gen_$(basename "$f")"; done \
    && ls /seeds/pdf | wc -l && du -sh /seeds/pdf

# Fuzzing (fuzz.yml): nightly and cargo-fuzz, with the targets built into the image. The
# workflow runs it with `docker run`, so a BuildKit cache hit cannot skip the fuzzing. The same
# stage builds natively on x86_64 and on aarch64.
FROM toolchain AS fuzz-tools
RUN rustup toolchain install nightly --profile minimal \
    && cargo install cargo-fuzz --locked --version 0.13.1

FROM fuzz-tools AS fuzz
COPY --from=deps /usr/local/cargo/registry /usr/local/cargo/registry
COPY --from=deps /usr/local/cargo/git /usr/local/cargo/git
ENV CARGO_NET_OFFLINE=true
COPY . .
COPY --from=fuzz-seeds /seeds/pdf tests/fuzz/seeds/pdf
RUN mkdir -p tests/fuzz/corpus tests/fuzz/artifacts \
    && cp tests/data/filter/*.txt tests/fuzz/seeds/parse_content/ \
    && cargo +nightly fuzz build --fuzz-dir tests/fuzz


# Benchmark (bench.yml): the Go oracle, the Rust bench example and the generated inputs in one
# image, so the two sides run on the same machine. `oracle -genbench` writes the inputs by class
# (docs/benchmarks.md).
FROM go-oracle AS go-bench
ENV GOMEMLIMIT=3GiB
COPY tests/data tests/data
RUN /out/oracle -genbench tests/data /bench-in && du -sh /bench-in

FROM src AS bench
COPY --from=go-oracle /out/oracle /usr/local/bin/oracle
COPY --from=go-bench /bench-in /bench-in
RUN --mount=type=cache,id=pdf-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo build --locked --release --example bench && cp target/release/examples/bench /usr/local/bin/bench
