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
