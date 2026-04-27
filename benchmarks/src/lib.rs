use libsrs_contract::{Packet, StreamId, Timebase, Timestamp};

pub fn synthetic_packet(seed: u8) -> Packet {
    Packet {
        stream_id: StreamId(0),
        pts: Some(Timestamp::new(0, Timebase::milliseconds())),
        dts: None,
        duration: None,
        keyframe: true,
        data: vec![seed; 256],
    }
}
