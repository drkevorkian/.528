use std::io::{self, Seek, SeekFrom, Write};

use libsrs_container::{
    encode_cue_block, encode_file_header, encode_index_block, encode_packet_block,
    encode_track_descriptor, write_all, CueBlock, FileHeader, IndexBlock, IndexEntry, Packet,
    PacketFlags, PacketHeader, TrackDescriptor,
};

pub struct MuxWriter<W: Write + Seek> {
    writer: W,
    header: FileHeader,
    tracks: Vec<TrackDescriptor>,
    packet_count: u64,
    cue_count: u64,
    sequence: u64,
    entries: Vec<IndexEntry>,
    pending_cue_start: usize,
}

impl<W: Write + Seek> MuxWriter<W> {
    pub fn new(
        mut writer: W,
        mut header: FileHeader,
        tracks: Vec<TrackDescriptor>,
    ) -> io::Result<Self> {
        header.track_count = u16::try_from(tracks.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many tracks"))?;

        write_all(&mut writer, &encode_file_header(&header))?;
        for track in &tracks {
            write_all(&mut writer, &encode_track_descriptor(track)?)?;
        }

        Ok(Self {
            writer,
            header,
            tracks,
            packet_count: 0,
            cue_count: 0,
            sequence: 0,
            entries: Vec::new(),
            pending_cue_start: 0,
        })
    }

    pub fn tracks(&self) -> &[TrackDescriptor] {
        &self.tracks
    }

    pub fn write_packet(
        &mut self,
        track_id: u16,
        pts: u64,
        dts: u64,
        keyframe: bool,
        payload: &[u8],
    ) -> io::Result<()> {
        let mut flags = 0u16;
        if keyframe {
            flags |= PacketFlags::KEYFRAME;
        }
        let packet = Packet {
            header: PacketHeader {
                track_id,
                flags,
                sequence: self.sequence,
                pts,
                dts,
                payload_len: u32::try_from(payload.len()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "payload too large")
                })?,
            },
            payload: payload.to_vec(),
        };

        let packet_offset = self.writer.stream_position()?;
        let block = encode_packet_block(&packet)?;
        write_all(&mut self.writer, &block)?;

        self.entries.push(IndexEntry {
            packet_number: self.packet_count,
            file_offset: packet_offset,
            track_id,
            flags,
            pts,
        });
        self.packet_count += 1;
        self.sequence += 1;

        if self.header.cue_interval_packets > 0
            && self.packet_count % self.header.cue_interval_packets as u64 == 0
        {
            self.write_periodic_cue()?;
        }

        Ok(())
    }

    fn write_periodic_cue(&mut self) -> io::Result<()> {
        let entries = self.entries[self.pending_cue_start..].to_vec();
        let first_packet_number = entries
            .first()
            .map_or(self.packet_count, |e| e.packet_number);
        let cue = CueBlock {
            cue_id: self.cue_count,
            first_packet_number,
            entries,
        };
        write_all(&mut self.writer, &encode_cue_block(&cue)?)?;
        self.cue_count += 1;
        self.pending_cue_start = self.entries.len();
        Ok(())
    }

    pub fn finalize(mut self) -> io::Result<W> {
        if self.pending_cue_start < self.entries.len() {
            self.write_periodic_cue()?;
        }
        let index = IndexBlock {
            entries: self.entries.clone(),
        };
        write_all(&mut self.writer, &encode_index_block(&index)?)?;
        self.writer.flush()?;
        self.writer.seek(SeekFrom::Start(0))?;
        Ok(self.writer)
    }
}
