use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;

use criterion::{criterion_group, criterion_main, Criterion};
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_mux::MuxWriter;
use libsrs_pipeline::{NoopNativeTranscoder, TranscodePipeline};
use libsrs_video::{encode_frame, FrameType, VideoFrame};

/// One-shot temp `.528` for stub ingest (synthetic `samples/synthetic.input` is no longer used).
fn minimal_528_path() -> PathBuf {
    static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut guard = PATH.lock().unwrap();
    if let Some(p) = guard.clone() {
        return p;
    }
    let path = std::env::temp_dir().join("srs-bench-seek-minimal.528");
    let w = 16u32;
    let video = VideoFrame {
        width: w,
        height: w,
        frame_index: 0,
        frame_type: FrameType::I,
        data: vec![0x55; (w * w) as usize],
    };
    let enc = encode_frame(&video).expect("encode bench frame");
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 1,
        flags: 0,
        timescale: 90_000,
        config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
    }];
    let file = File::create(&path).expect("bench temp mux");
    let mut mux = MuxWriter::new(file, FileHeader::new(1, 4), tracks).expect("mux");
    mux.write_packet(1, 0, 0, true, &enc).expect("packet");
    mux.finalize().expect("finalize");
    *guard = Some(path.clone());
    path
}

fn bench_seek_latency_hooks(c: &mut Criterion) {
    c.bench_function("seek_latency_stub_import", |b| {
        let path = minimal_528_path();
        b.iter(|| {
            let pipeline = TranscodePipeline::default();
            let mut native = NoopNativeTranscoder::default();
            let _ = pipeline.import_to_native(&path, &mut native);
        })
    });
}

criterion_group!(benches, bench_seek_latency_hooks);
criterion_main!(benches);
