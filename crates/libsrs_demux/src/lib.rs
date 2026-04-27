use std::io::{self, ErrorKind, Read, Seek, SeekFrom};

use libsrs_container::{
    decode_block_header, decode_cue_block, decode_file_header, decode_index_block,
    decode_packet_block, decode_track_descriptor, find_next_block_magic, read_block_body,
    BlockType, FileHeader, IndexBlock, IndexEntry, Packet, TrackDescriptor, BLOCK_MAGIC,
};

#[derive(Debug, Clone)]
pub struct DemuxedPacket {
    pub offset: u64,
    pub packet: Packet,
}

pub struct DemuxReader<R: Read + Seek> {
    reader: R,
    header: FileHeader,
    tracks: Vec<TrackDescriptor>,
    data_start_offset: u64,
    packet_cursor: u64,
    index: Vec<IndexEntry>,
}

impl<R: Read + Seek> DemuxReader<R> {
    pub fn open(mut reader: R) -> io::Result<Self> {
        let header = decode_file_header(&mut reader)?;
        let mut tracks = Vec::with_capacity(header.track_count as usize);
        for _ in 0..header.track_count {
            tracks.push(decode_track_descriptor(&mut reader)?);
        }
        let data_start_offset = reader.stream_position()?;
        Ok(Self {
            reader,
            header,
            tracks,
            data_start_offset,
            packet_cursor: 0,
            index: Vec::new(),
        })
    }

    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    pub fn tracks(&self) -> &[TrackDescriptor] {
        &self.tracks
    }

    pub fn index(&self) -> &[IndexEntry] {
        &self.index
    }

    pub fn reset_to_data_start(&mut self) -> io::Result<()> {
        self.reader.seek(SeekFrom::Start(self.data_start_offset))?;
        self.packet_cursor = 0;
        Ok(())
    }

    pub fn seek_nearest(&mut self, pts: u64) -> io::Result<Option<IndexEntry>> {
        if self.index.is_empty() {
            self.rebuild_index()?;
        }
        let selected = self
            .index
            .iter()
            .filter(|entry| entry.pts <= pts)
            .max_by_key(|entry| entry.pts)
            .cloned();
        if let Some(entry) = &selected {
            self.reader.seek(SeekFrom::Start(entry.file_offset))?;
        }
        Ok(selected)
    }

    pub fn rebuild_index(&mut self) -> io::Result<()> {
        let current = self.reader.stream_position()?;
        self.reader.seek(SeekFrom::Start(self.data_start_offset))?;
        self.index.clear();
        while let Some(result) = self.next_block()? {
            match result {
                ParsedBlock::Packet(offset, packet) => {
                    self.index.push(IndexEntry {
                        packet_number: packet.header.sequence,
                        file_offset: offset,
                        track_id: packet.header.track_id,
                        flags: packet.header.flags,
                        pts: packet.header.pts,
                    });
                }
                ParsedBlock::Cue(cue) => {
                    self.index.extend(cue.entries);
                }
                ParsedBlock::Index(IndexBlock { entries }) => {
                    self.index = entries;
                    break;
                }
            }
        }
        self.reader.seek(SeekFrom::Start(current))?;
        Ok(())
    }

    pub fn next_packet(&mut self) -> io::Result<Option<DemuxedPacket>> {
        loop {
            let Some(block) = self.next_block()? else {
                return Ok(None);
            };
            if let ParsedBlock::Packet(offset, packet) = block {
                self.packet_cursor += 1;
                return Ok(Some(DemuxedPacket { offset, packet }));
            }
        }
    }

    fn next_block(&mut self) -> io::Result<Option<ParsedBlock>> {
        let block_start = self.reader.stream_position()?;
        let header = match decode_block_header(&mut self.reader) {
            Ok(value) => value,
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return self.try_resync_or_fail(block_start, err),
        };
        let body = read_block_body(&mut self.reader, &header)?;
        match header.block_type {
            BlockType::Packet => Ok(Some(ParsedBlock::Packet(
                block_start,
                decode_packet_block(&body)?,
            ))),
            BlockType::Cue => Ok(Some(ParsedBlock::Cue(decode_cue_block(&body)?))),
            BlockType::Index => Ok(Some(ParsedBlock::Index(decode_index_block(&body)?))),
        }
    }

    fn try_resync_or_fail(
        &mut self,
        block_start: u64,
        original_error: io::Error,
    ) -> io::Result<Option<ParsedBlock>> {
        let mut probe = vec![0u8; 8192];
        self.reader.seek(SeekFrom::Start(block_start))?;
        let read = self.reader.read(&mut probe)?;
        if read < BLOCK_MAGIC.len() {
            return Err(original_error);
        }
        if let Some(offset) = find_next_block_magic(&probe[..read], 1) {
            self.reader
                .seek(SeekFrom::Start(block_start + offset as u64))?;
            return self.next_block();
        }
        Err(original_error)
    }
}

enum ParsedBlock {
    Packet(u64, Packet),
    Cue(libsrs_container::CueBlock),
    Index(IndexBlock),
}
