# pooled-writer

<p align="center">
  <a href="https://github.com/fulcrumgenomics/pooled-writer/actions?query=workflow%3ACheck"><img src="https://github.com/fulcrumgenomics/pooled-writer/actions/workflows/build_and_test.yml/badge.svg" alt="Build Status"></a>
  <img src="https://img.shields.io/crates/l/read_structure.svg" alt="license">
  <a href="https://crates.io/crates/pooled-writer"><img src="https://img.shields.io/crates/v/pooled-writer.svg?colorB=319e8c" alt="Version info"></a><br>
</p>

A pooled writer and compressor.

This library is intended for scenarios where the number of writers you have is >= the number of threads you want to use for writing.

Note that this is an alpha release and the API could change drastically in future releases.

## Documentation and Examples

Please see the generated [Rust Docs](https://docs.rs/pooled-writer).
