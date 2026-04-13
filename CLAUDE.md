# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build, Test, and Lint Commands

```bash
cargo build                    # Build the library
cargo test                     # Run tests (skips the long-running property-based test)
cargo test -- --ignored        # Run the comprehensive property-based test (~10-20 min)
cargo test <test_name>         # Run a single test by name
cargo fmt --all -- --check     # Check formatting
cargo clippy -- -D warnings    # Lint (all warnings are errors in CI)
```

Rust toolchain is pinned to 1.85.0 (2024 edition) via `rust-toolchain.toml`.

## Architecture

`pooled-writer` solves the problem of compressing and writing data to M output writers using N threads, where M != N (e.g., hundreds of gzipped files with 16 threads, or 4 files with 32 threads).

### Core types (all in `src/lib.rs`)

- **`Compressor`** trait -- abstraction over compression algorithms. Has an associated `BLOCK_SIZE` that controls buffering. Implementations must be `Send + 'static`.
- **`BgzfCompressor`** (`src/bgzf.rs`) -- the default `Compressor` implementation for BGZF format, feature-gated behind `bgzf_compressor` (on by default).
- **`PoolBuilder<W, C>`** -- builder pattern to configure threads, queue size, and compression level. Writers are swapped for `PooledWriter`s via `exchange()`, then `build()` starts the pool.
- **`Pool`** -- owns the thread pool and orchestrates all work. A single management thread spawns N worker threads. Workers loop: try to compress one block, then try to write one block.
- **`PooledWriter`** -- implements `std::io::Write`. Buffers data internally; when the buffer hits `BLOCK_SIZE`, it sends the block through channels for compression and writing. Must be `close()`d or `drop()`d before calling `Pool::stop_pool()`.

### Concurrency model

All concurrency uses message passing (flume channels), not shared mutable state. Per-writer output channels maintain write ordering. One-shot channels link each compression result back to its writer queue. A third notification channel prevents polling across potentially many per-writer channels.

### Key invariant

`#![forbid(unsafe_code)]` -- no unsafe code anywhere in the crate.

## Code Style

- Max line width: 100 characters (`rustfmt.toml`)
- Clippy with `-D warnings` (all warnings denied)
