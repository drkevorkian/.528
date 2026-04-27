use criterion::{criterion_group, criterion_main, Criterion};
use libsrs_pipeline::{NativeTranscoder, NoopNativeTranscoder};
use srs_benchmarks::synthetic_packet;

fn bench_encode_decode(c: &mut Criterion) {
    c.bench_function("encode_decode_noop", |b| {
        b.iter(|| {
            let mut native = NoopNativeTranscoder::default();
            for i in 0..128u8 {
                let packet = synthetic_packet(i);
                native
                    .transcode_packet(packet)
                    .expect("noop transcode should succeed");
            }
            native.finalize().expect("noop finalize should succeed");
        })
    });
}

criterion_group!(benches, bench_encode_decode);
criterion_main!(benches);
