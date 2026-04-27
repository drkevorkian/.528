use criterion::{criterion_group, criterion_main, Criterion};
use libsrs_pipeline::{NoopNativeTranscoder, TranscodePipeline};

fn bench_seek_latency_hooks(c: &mut Criterion) {
    c.bench_function("seek_latency_stub_import", |b| {
        b.iter(|| {
            let pipeline = TranscodePipeline::default();
            let mut native = NoopNativeTranscoder::default();
            let _ = pipeline.import_to_native("samples/synthetic.input", &mut native);
        })
    });
}

criterion_group!(benches, bench_seek_latency_hooks);
criterion_main!(benches);
