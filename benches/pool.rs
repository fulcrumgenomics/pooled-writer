use std::io::Write;
use std::sync::{Arc, Mutex};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use pooled_writer::PoolBuilder;
use pooled_writer::bgzf::BgzfCompressor;

/// The number of FASTQ records to generate per writer.  Each record is ~250 bytes so
/// 20_000 records is ~5 MB of uncompressed data per writer.
const RECORDS_PER_WRITER: usize = 20_000;

/// Generates synthetic FASTQ data that approximates the compression profile of real
/// Illumina sequencing data.  Returns a single byte buffer containing `num_records`
/// FASTQ records with 150bp reads.
///
/// The data is pseudo-random but deterministic (seeded) so benchmarks are reproducible.
fn generate_fastq_data(num_records: usize, seed: u64) -> Vec<u8> {
    // Simple deterministic PRNG (xorshift64) so we don't need the rand crate
    let mut state = seed ^ 0x5DEE_CE66_D1A4_F87D;
    let mut next_u64 = || -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let bases = b"ACGTACGTACGTACGN"; // N at ~6% frequency
    let read_len = 150;

    // Pre-size: each record is ~(read_len * 2 + 60) bytes for header, seq, +, qual, newlines
    let mut buf = Vec::with_capacity(num_records * (read_len * 2 + 60));

    for i in 0..num_records {
        // Header line -- realistic instrument/flowcell/coordinates format
        buf.extend_from_slice(b"@SIM:001:HXXXX:1:1101:");
        buf.extend_from_slice(i.to_string().as_bytes());
        buf.extend_from_slice(b":1234 1:N:0:ACGTACGT\n");

        // Sequence line -- draw from bases with slight GC bias
        for _ in 0..read_len {
            let r = next_u64();
            buf.push(bases[(r & 0xF) as usize]);
        }
        buf.push(b'\n');

        // Separator
        buf.push(b'+');
        buf.push(b'\n');

        // Quality line -- Illumina-like: starts high, drops toward 3' end
        for pos in 0..read_len {
            // Base quality degrades with position: Phred ~35 at start, ~20 at end,
            // with noise.  Clamped to Illumina range '!' (33) to 'J' (74).
            let base_q = 35.0 - (pos as f64 / read_len as f64) * 15.0;
            let noise = ((next_u64() % 11) as f64) - 5.0; // -5..+5
            let q = (base_q + noise).clamp(2.0, 41.0) as u8;
            buf.push(q + 33); // Phred+33 encoding
        }
        buf.push(b'\n');
    }

    buf
}

/// Benchmark throughput writing to in-memory writers, varying number of threads.
fn bench_thread_scaling(c: &mut Criterion) {
    let num_writers = 8;
    let data: Vec<Vec<u8>> =
        (0..num_writers).map(|i| generate_fastq_data(RECORDS_PER_WRITER, i as u64)).collect();
    let total_bytes: u64 = data.iter().map(|d| d.len() as u64).sum();

    let mut group = c.benchmark_group("thread_scaling");
    group.throughput(Throughput::Bytes(total_bytes));

    for threads in [1, 2, 4, 8] {
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, &threads| {
            b.iter(|| {
                let writers: Vec<Arc<Mutex<Vec<u8>>>> =
                    (0..num_writers).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
                let mut builder = PoolBuilder::<_, BgzfCompressor>::new()
                    .threads(threads)
                    .compression_level(2)
                    .unwrap();
                let mut pooled: Vec<_> =
                    writers.iter().map(|w| builder.exchange(ArcVecWriter(Arc::clone(w)))).collect();
                let mut pool = builder.build().unwrap();

                for (pw, input) in pooled.iter_mut().zip(data.iter()) {
                    pw.write_all(input).unwrap();
                }
                pooled.into_iter().for_each(|w| w.close().unwrap());
                pool.stop_pool().unwrap();
            });
        });
    }

    group.finish();
}

/// Benchmark throughput with fixed threads, varying number of writers.
fn bench_writer_scaling(c: &mut Criterion) {
    let threads = 4;

    let mut group = c.benchmark_group("writer_scaling");

    for num_writers in [4, 16, 64] {
        let data: Vec<Vec<u8>> =
            (0..num_writers).map(|i| generate_fastq_data(RECORDS_PER_WRITER, i as u64)).collect();
        let total_bytes: u64 = data.iter().map(|d| d.len() as u64).sum();
        group.throughput(Throughput::Bytes(total_bytes));

        group.bench_with_input(
            BenchmarkId::new("writers", num_writers),
            &num_writers,
            |b, &num_writers| {
                b.iter(|| {
                    let writers: Vec<Arc<Mutex<Vec<u8>>>> =
                        (0..num_writers).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
                    let mut builder = PoolBuilder::<_, BgzfCompressor>::new()
                        .threads(threads)
                        .compression_level(2)
                        .unwrap();
                    let mut pooled: Vec<_> = writers
                        .iter()
                        .map(|w| builder.exchange(ArcVecWriter(Arc::clone(w))))
                        .collect();
                    let mut pool = builder.build().unwrap();

                    for (pw, input) in pooled.iter_mut().zip(data.iter()) {
                        pw.write_all(input).unwrap();
                    }
                    pooled.into_iter().for_each(|w| w.close().unwrap());
                    pool.stop_pool().unwrap();
                });
            },
        );
    }

    group.finish();
}

/// Benchmark throughput at different compression levels with fixed threads and writers.
fn bench_compression_levels(c: &mut Criterion) {
    let num_writers = 8;
    let threads = 4;
    let data: Vec<Vec<u8>> =
        (0..num_writers).map(|i| generate_fastq_data(RECORDS_PER_WRITER, i as u64)).collect();
    let total_bytes: u64 = data.iter().map(|d| d.len() as u64).sum();

    let mut group = c.benchmark_group("compression_levels");
    group.throughput(Throughput::Bytes(total_bytes));

    for level in [1, 2, 5, 8] {
        group.bench_with_input(BenchmarkId::new("level", level), &level, |b, &level| {
            b.iter(|| {
                let writers: Vec<Arc<Mutex<Vec<u8>>>> =
                    (0..num_writers).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
                let mut builder = PoolBuilder::<_, BgzfCompressor>::new()
                    .threads(threads)
                    .compression_level(level)
                    .unwrap();
                let mut pooled: Vec<_> =
                    writers.iter().map(|w| builder.exchange(ArcVecWriter(Arc::clone(w)))).collect();
                let mut pool = builder.build().unwrap();

                for (pw, input) in pooled.iter_mut().zip(data.iter()) {
                    pw.write_all(input).unwrap();
                }
                pooled.into_iter().for_each(|w| w.close().unwrap());
                pool.stop_pool().unwrap();
            });
        });
    }

    group.finish();
}

/// An owned `Write` adapter backed by an `Arc<Mutex<Vec<u8>>>`.
/// Lets benchmarks use in-memory writers that satisfy the `'static` bound
/// required by `PoolBuilder::exchange` without disk I/O noise.
struct ArcVecWriter(Arc<Mutex<Vec<u8>>>);

impl Write for ArcVecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

criterion_group!(benches, bench_thread_scaling, bench_writer_scaling, bench_compression_levels);
criterion_main!(benches);
