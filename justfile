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

# Sampling flamegraph via samply (Firefox Profiler). Opens the viewer
# automatically when the run finishes.
#
# Cold by default — that's where the parser hot paths show up. For warm-path
# profiling (cache-hit traversal), pass `cold=0`: `just profile-flamegraph cold=0`.
profile-flamegraph cold="1": build _check-meow-client
    @command -v samply >/dev/null || (echo "samply not installed (cargo install samply)" && exit 1)
    @mkdir -p {{PROFILE_OUT}}
    @if [ "{{cold}}" = "1" ]; then \
      rm -f {{PROFILE_OUT}}/asset-db.bin {{PROFILE_OUT}}/asset-db.cache.bin; \
    fi
    samply record --save-only --output {{PROFILE_OUT}}/profile.json \
      {{BIN}} bake --project {{MEOW_CLIENT}} --out-dir {{PROFILE_OUT}}
    @echo "Saved: {{PROFILE_OUT}}/profile.json"
    @echo "View:  samply load {{PROFILE_OUT}}/profile.json"

# Internal: bail if the meow-tower checkout isn't at MEOW_CLIENT.
_check-meow-client:
    @test -d "{{MEOW_CLIENT}}/Assets" || ( \
      echo "MEOW_CLIENT={{MEOW_CLIENT}} doesn't contain Assets/ — set MEOW_CLIENT to your Unity project root" \
      && exit 1)
