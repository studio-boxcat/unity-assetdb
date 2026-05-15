# unity-assetdb dev recipes. See [[docs/profiling.md]] for the profile/* set.

BIN := "target/release/unity-assetdb"

# Default Unity project for the profile recipes — meow-tower. Override on the
# command line: `just MEOW_CLIENT=/path/to/other/project profile`.
MEOW_CLIENT := env_var_or_default("MEOW_CLIENT", "/Users/jameskim/Develop/meow-tower")

# Out dir for the bake during profiling — kept off-tree so iterating doesn't
# touch fixtures.
PROFILE_OUT := "/tmp/unity-assetdb-profile"

default:
    @just --list

# Build the release binary.
build:
    cargo build --release

# Install the CLI to ~/.cargo/bin (on PATH).
install:
    cargo install --path .

# Run lib + integration tests.
test:
    cargo test

# Lint.
lint:
    cargo clippy --all-targets

# --- profiling ----------------------------------------------------------

# Run all microbench examples (release-mode, isolated phase costs).
bench: build _check-meow-client
    cargo build --release --example bench_list --example bench_bake
    @echo "=== bench_bake ==="
    MEOW_CLIENT={{MEOW_CLIENT}} target/release/examples/bench_bake
    @echo "=== bench_list ==="
    target/release/examples/bench_list

# A/B compare two refs. Builds each commit's binary in a worktree to avoid
# churning the working tree, then runs hyperfine cold+warm against both.
# Usage: `just compare 6632c12 HEAD` (refs/SHAs accepted).
compare BEFORE AFTER: _check-meow-client
    @command -v hyperfine >/dev/null || (echo "hyperfine not installed (brew install hyperfine)" && exit 1)
    @mkdir -p {{PROFILE_OUT}}
    @rm -rf /tmp/uadb-compare-{{BEFORE}} /tmp/uadb-compare-{{AFTER}}
    git worktree add --detach /tmp/uadb-compare-before {{BEFORE}}
    git worktree add --detach /tmp/uadb-compare-after  {{AFTER}}
    cd /tmp/uadb-compare-before && cargo build --release --quiet --bin unity-assetdb
    cd /tmp/uadb-compare-after  && cargo build --release --quiet --bin unity-assetdb
    @cp /tmp/uadb-compare-before/target/release/unity-assetdb /tmp/uadb-before
    @cp /tmp/uadb-compare-after/target/release/unity-assetdb /tmp/uadb-after
    git worktree remove /tmp/uadb-compare-before
    git worktree remove /tmp/uadb-compare-after
    @echo "=== cold ({{BEFORE}} vs {{AFTER}}) ==="
    hyperfine --warmup 2 --runs 5 \
      --prepare "rm -f {{PROFILE_OUT}}/asset-db.bin {{PROFILE_OUT}}/asset-db.cache.bin" \
      "/tmp/uadb-before bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}" \
      "/tmp/uadb-after  bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}"
    @echo "=== warm ({{BEFORE}} vs {{AFTER}}) ==="
    hyperfine --warmup 3 --runs 10 \
      "/tmp/uadb-before bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}" \
      "/tmp/uadb-after  bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}"

# Cold + warm wall-clock runs via hyperfine, then a per-phase line.
profile: build _check-meow-client
    @command -v hyperfine >/dev/null || (echo "hyperfine not installed (brew install hyperfine)" && exit 1)
    @mkdir -p {{PROFILE_OUT}}
    @echo "=== cold (no cache, no db) ==="
    hyperfine --warmup 2 --runs 5 \
      --prepare "rm -f {{PROFILE_OUT}}/asset-db.bin {{PROFILE_OUT}}/asset-db.cache.bin" \
      "{{BIN}} bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}"
    @echo "=== warm (full cache hit) ==="
    hyperfine --warmup 2 --runs 5 \
      "{{BIN}} bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}"
    @echo "=== phase breakdown (warm) ==="
    UNITY_ASSETDB_TIMING=1 {{BIN}} bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}
    @ls -lh {{PROFILE_OUT}}

# Internal: bail if the meow-tower checkout isn't at MEOW_CLIENT.
_check-meow-client:
    @test -d "{{MEOW_CLIENT}}/Assets" || ( \
      echo "MEOW_CLIENT={{MEOW_CLIENT}} doesn't contain Assets/ — set MEOW_CLIENT to your Unity project root" \
      && exit 1)
