use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use libsrs_bitio::{
    encode_u64_varint_into, rans_decode, rans_encode, BitReader, BitWriter, RansModel,
};

fn bench_varint_encode(c: &mut Criterion) {
    c.bench_function("varint_encode_u64_mixed", |b| {
        b.iter(|| {
            let mut v = Vec::<u8>::with_capacity(32);
            for n in 0_u64..32 {
                encode_u64_varint_into(&mut v, n * 1_000_003).unwrap();
            }
            v
        })
    });
}

fn bench_bitpack_msb(c: &mut Criterion) {
    c.bench_function("bitwriter_reader_msb_64k_bits", |b| {
        b.iter(|| {
            let mut w = BitWriter::with_capacity(8192);
            for i in 0_u64..8192 {
                let _ = w.write(8, (i as u8) as u64);
            }
            let buf = w.finish();
            let mut r = BitReader::new(&buf);
            for _ in 0..8192 {
                let _ = r.read(8).unwrap();
            }
            buf.len()
        })
    });
}

fn bench_rans_roundtrip(c: &mut Criterion) {
    let model = RansModel::uniform(256).unwrap();
    let symbols: Vec<usize> = (0..512).map(|i| i % 256).collect();
    let enc = rans_encode(&model, &symbols).unwrap();
    let budget = enc.len().saturating_mul(16);
    c.bench_function("rans_roundtrip_512_bytes", |b| {
        b.iter(|| {
            let out = rans_decode(&model, &enc, symbols.len(), budget).unwrap();
            assert_eq!(out.len(), symbols.len());
            out
        })
    });
}

fn configure() -> Criterion {
    Criterion::default().measurement_time(Duration::from_secs(2))
}

criterion_group!(
    name = bitio;
    config = configure();
    targets = bench_varint_encode, bench_bitpack_msb, bench_rans_roundtrip
);
criterion_main!(bitio);
