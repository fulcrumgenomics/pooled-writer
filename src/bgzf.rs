///! An implementation of [`Compressor`] for the `BGZF` format.
use crate::Compressor;

/// A BGZF compressor.
pub struct BgzfCompressor {
    inner: bgzf::Compressor,
}

impl Compressor for BgzfCompressor {
    type Error = bgzf::BgzfError;
    type CompressionLevel = bgzf::CompressionLevel;

    const BLOCK_SIZE: usize = bgzf::BGZF_BLOCK_SIZE;

    fn new(compression_level: Self::CompressionLevel) -> Self {
        Self { inner: bgzf::Compressor::new(compression_level) }
    }

    fn default_compression_level() -> Self::CompressionLevel {
        bgzf::CompressionLevel::new(5).unwrap()
    }

    fn new_compression_level(compression_level: u8) -> Result<Self::CompressionLevel, Self::Error> {
        bgzf::CompressionLevel::new(compression_level)
    }

    fn compress(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        is_last: bool,
    ) -> Result<(), Self::Error> {
        self.inner.compress(input, output)?;
        if is_last {
            bgzf::Compressor::append_eof(output);
        }
        Ok(())
    }
}
