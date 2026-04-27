#![no_main]

use libfuzzer_sys::fuzz_target;
use srs_fuzz::demux_reader_skeleton;

fuzz_target!(|data: &[u8]| {
    demux_reader_skeleton(data);
});
