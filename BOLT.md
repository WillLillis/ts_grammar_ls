# BOLT Optimization for the Grammar DSL Language Server

## Background

The nativedsl `parse_native_dsl` function runs ~25% slower inside a large
binary compared to a small standalone binary due to L1 icache pressure.
`perf stat` shows 27M icache misses in a 13MB binary vs 75K in a 3.6MB binary
for the same code and input.

BOLT (Binary Optimization and Layout Tool) reorders functions and basic blocks
based on a runtime profile, packing hot code together to improve cache locality.
In testing on the tree-sitter CLI, BOLT achieved:
- 22% reduction in L1 icache misses (27.3M -> 21.2M)
- 43.5% reduction in taken branches
- 51.5% reduction in taken conditional branches

These improvements didn't translate to measurable wall-clock gains for
`tree-sitter generate` (where DSL parsing is <0.02% of total time), but
for an LSP that parses grammars on every keystroke, the icache improvement
could be significant.

## Prerequisites

```bash
# CachyOS / Arch
sudo pacman -S llvm-bolt

# Ubuntu / Debian
sudo apt-get install llvm-bolt
```

Verify: `llvm-bolt --version`

## Steps

### 1. Build with relocations

BOLT needs relocation info in the binary. Add `--emit-relocs` to the linker:

```bash
RUSTFLAGS="-C link-args=-Wl,--emit-relocs" cargo build --release
```

The binary must NOT be stripped (`strip = false` in Cargo.toml release profile).

### 2. Collect a runtime profile

Use `perf` with branch recording. Run a representative workload - for the LSP,
this means opening/editing several grammar files:

```bash
perf record -e cycles:u -j any,u -o /tmp/bolt-perf.data -- \
  target/release/ts-grammar-ls --stdio < /tmp/lsp-workload.json
```

If simulating a workload is hard, you can use the test suite:

```bash
perf record -e cycles:u -j any,u -o /tmp/bolt-perf.data -- \
  cargo test --release
```

More samples = better results. BOLT will warn if it needs more.

### 3. Convert perf data to BOLT format

```bash
perf2bolt target/release/ts-grammar-ls \
  -p /tmp/bolt-perf.data \
  -o /tmp/bolt.fdata
```

Look for the "estimated to optimize better with Nx more samples" warning.
If N > 5, collect more profile data.

### 4. Apply BOLT optimization

```bash
llvm-bolt target/release/ts-grammar-ls \
  -data /tmp/bolt.fdata \
  -o target/release/ts-grammar-ls-bolted \
  -reorder-blocks=ext-tsp \
  -reorder-functions=hfsort \
  -split-functions \
  -split-all-cold \
  -dyno-stats
```

Key flags:
- `-reorder-blocks=ext-tsp` - reorder basic blocks within functions using
  the Extended TSP algorithm (best for icache)
- `-reorder-functions=hfsort` - reorder functions using the HFSort algorithm
  (groups frequently co-called functions together)
- `-split-functions` - move cold code out of hot functions
- `-split-all-cold` - aggressively split cold blocks
- `-dyno-stats` - print before/after statistics

### 5. Verify

```bash
# Quick sanity check
./target/release/ts-grammar-ls-bolted --version

# Compare icache misses
perf stat -e L1-icache-load-misses target/release/ts-grammar-ls <workload>
perf stat -e L1-icache-load-misses target/release/ts-grammar-ls-bolted <workload>
```

## Expected impact

For a binary that repeatedly calls `parse_native_dsl` (like the LSP on
every file change), expect:
- 20-30% fewer L1 icache misses
- Potentially 10-15% faster repeated parse_native_dsl calls
- No change for cold (first) invocations

The impact scales with binary size - larger binaries benefit more because
there's more cold code polluting the icache.

## Integration into CI/release

BOLT can be added as a post-build step:

```bash
# In a release script:
RUSTFLAGS="-C link-args=-Wl,--emit-relocs" cargo build --release
# ... collect profile ...
llvm-bolt target/release/ts-grammar-ls \
  -data profile.fdata \
  -o target/release/ts-grammar-ls \
  -reorder-blocks=ext-tsp \
  -reorder-functions=hfsort \
  -split-functions \
  -split-all-cold
strip target/release/ts-grammar-ls  # safe to strip after BOLT
```

The profile data needs to be representative of real usage. A stale profile
is better than no profile, but re-profiling after major code changes is
recommended.
