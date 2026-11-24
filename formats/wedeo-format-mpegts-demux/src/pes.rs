//! PES (Packetized Elementary Stream) assembly.
//!
//! Ported from FFmpeg's `mpegts_push_data()` in `libavformat/mpegts.c` and
//! `ff_parse_pes_pts()` in `libavformat/mpeg.h`.

use wedeo_core::packet::{Packet, PacketFlags, PacketSideData, PacketSideDataType};
use wedeo_core::timestamp::NOPTS_VALUE;

use crate::constants::{MAX_PES_HEADER_SIZE, PES_HEADER_SIZE, PES_START_SIZE, stream_id};

/// `enum MpegTSState` in FFmpeg `mpegts.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PesState {
    Header,
    PesHeader,
    PesHeaderFill,
    Payload,
    Skip,
}

/// `PESContext` in FFmpeg `mpegts.c`, reduced to the fields needed for
/// assembling PES packets out of TS payloads.
#[derive(Debug, Clone)]
pub struct PesContext {
    pub stream_index: Option<usize>,
    pub state: PesState,
    pub data_index: usize,
    pub flags: PacketFlags,
    pub pes_packet_length: usize,
    pub pes_header_size: usize,
    pub stream_id: u8,
    pub extended_stream_id: i32,
    pub pts: i64,
    pub dts: i64,
    /// Position of the first TS packet of this PES packet.
    pub ts_packet_pos: i64,
    pub header: Vec<u8>,
    pub buffer: Vec<u8>,
    /// FFmpeg's `ts->stop_parse`: set when a packet is completed so the
    /// demuxer stops reading TS packets for this `read_packet()` call.
    pub stop_parse: bool,
}

impl PesContext {
    pub fn new(stream_index: Option<usize>) -> Self {
        Self {
            stream_index,
            state: PesState::Skip,
            data_index: 0,
            flags: PacketFlags::empty(),
            pes_packet_length: 0,
            pes_header_size: 0,
            stream_id: 0,
            extended_stream_id: -1,
            pts: NOPTS_VALUE,
            dts: NOPTS_VALUE,
            ts_packet_pos: -1,
            header: vec![0u8; MAX_PES_HEADER_SIZE],
            buffer: Vec::new(),
            stop_parse: false,
        }
    }

    /// `reset_pes_packet_state()` in FFmpeg `mpegts.c`.
    fn reset_packet_state(&mut self) {
        self.pts = NOPTS_VALUE;
        self.dts = NOPTS_VALUE;
        self.data_index = 0;
        self.flags = PacketFlags::empty();
        self.buffer.clear();
    }

    /// Drop any partial payload, leaving the stream association intact.
    /// Used after a seek is detected (FFmpeg `handle_packets()`).
    pub fn flush(&mut self) {
        self.buffer.clear();
        self.data_index = 0;
        self.state = PesState::Skip;
        self.pts = NOPTS_VALUE;
        self.dts = NOPTS_VALUE;
        self.flags = PacketFlags::empty();
    }

    /// Build an `AVPacket` out of the accumulated payload (`new_pes_packet()`).
    fn take_packet(&mut self) -> Packet {
        let mut pkt = Packet::from_slice(&self.buffer);
        pkt.pts = self.pts;
        pkt.dts = self.dts;
        pkt.stream_index = self.stream_index.unwrap_or(0);
        /* store position of first TS packet of this PES packet */
        pkt.pos = self.ts_packet_pos;
        pkt.flags = self.flags;

        // FFmpeg attaches AV_PKT_DATA_MPEGTS_STREAM_ID side data to every PES
        // packet. Our PacketSideDataType::MpegtsStreamId matches its ordinal.
        pkt.side_data.push(PacketSideData {
            data_type: PacketSideDataType::MpegtsStreamId,
            data: vec![self.stream_id],
        });

        self.buffer.clear();
        self.data_index = 0;
        self.pts = NOPTS_VALUE;
        self.dts = NOPTS_VALUE;
        self.flags = PacketFlags::empty();
        pkt
    }

    /// Return a pending PES payload, if any. Called at EOF to flush the last
    /// packet, mirroring `mpegts_read_packet()`'s flush path.
    pub fn flush_packet(&mut self) -> Option<Packet> {
        if self.state == PesState::Payload && !self.buffer.is_empty() {
            self.state = PesState::Skip;
            Some(self.take_packet())
        } else {
            None
        }
    }

    /// Feed TS payload bytes into the PES state machine.
    ///
    /// Returns all packets completed while processing `buf`. Mirrors
    /// `mpegts_push_data()`; `pos` is the position of the first TS packet of
    /// this PES packet (FFmpeg's `ts_packet_pos`).
    pub fn push_data(
        &mut self,
        mut buf: &[u8],
        is_start: bool,
        pos: i64,
        max_packet_size: usize,
    ) -> Vec<Packet> {
        let mut out = Vec::new();

        if is_start {
            if self.state == PesState::Payload && !self.buffer.is_empty() {
                out.push(self.take_packet());
                self.stop_parse = true;
            } else {
                self.reset_packet_state();
            }
            self.state = PesState::Header;
            self.ts_packet_pos = pos;
        }

        while !buf.is_empty() {
            match self.state {
                PesState::Header => {
                    let len = (PES_START_SIZE - self.data_index).min(buf.len());
                    self.header[self.data_index..self.data_index + len]
                        .copy_from_slice(&buf[..len]);
                    self.data_index += len;
                    buf = &buf[len..];
                    if self.data_index == PES_START_SIZE {
                        /* we got all the PES or section header. We can now
                         * decide */
                        if self.header[0] == 0x00
                            && self.header[1] == 0x00
                            && self.header[2] == 0x01
                        {
                            /* it must be an MPEG-2 PES stream */
                            self.stream_id = self.header[3];

                            if self.stream_id == stream_id::PADDING_STREAM {
                                /* skip packet */
                                self.state = PesState::Skip;
                                continue;
                            }

                            /* NOTE: zero length means the PES size is unbounded */
                            self.pes_packet_length =
                                u16::from_be_bytes([self.header[4], self.header[5]]) as usize;

                            if self.stream_id != stream_id::PROGRAM_STREAM_MAP
                                && self.stream_id != stream_id::PRIVATE_STREAM_2
                                && self.stream_id != stream_id::ECM_STREAM
                                && self.stream_id != stream_id::EMM_STREAM
                                && self.stream_id != stream_id::PROGRAM_STREAM_DIRECTORY
                                && self.stream_id != stream_id::DSMCC_STREAM
                                && self.stream_id != stream_id::TYPE_E_STREAM
                            {
                                self.state = PesState::PesHeader;
                            } else {
                                self.pes_header_size = PES_START_SIZE;
                                self.state = PesState::Payload;
                                self.data_index = 0;
                            }
                        } else {
                            /* otherwise, it should be a table */
                            /* skip packet */
                            self.state = PesState::Skip;
                        }
                    }
                }
                PesState::PesHeader => {
                    let len = (PES_HEADER_SIZE - self.data_index).min(buf.len());
                    self.header[self.data_index..self.data_index + len]
                        .copy_from_slice(&buf[..len]);
                    self.data_index += len;
                    buf = &buf[len..];
                    if self.data_index == PES_HEADER_SIZE {
                        self.pes_header_size = self.header[8] as usize + 9;
                        self.state = PesState::PesHeaderFill;
                    }
                }
                PesState::PesHeaderFill => {
                    let len = self
                        .pes_header_size
                        .saturating_sub(self.data_index)
                        .min(buf.len());
                    self.header[self.data_index..self.data_index + len]
                        .copy_from_slice(&buf[..len]);
                    self.data_index += len;
                    buf = &buf[len..];
                    if self.data_index == self.pes_header_size {
                        let flags = self.header[7];
                        let mut r = 9usize;
                        self.pts = NOPTS_VALUE;
                        self.dts = NOPTS_VALUE;
                        if flags & 0xc0 == 0x80 {
                            self.dts = parse_pes_pts(&self.header[r..r + 5]);
                            self.pts = self.dts;
                            r += 5;
                        } else if flags & 0xc0 == 0xc0 {
                            self.pts = parse_pes_pts(&self.header[r..r + 5]);
                            r += 5;
                            self.dts = parse_pes_pts(&self.header[r..r + 5]);
                            r += 5;
                        }
                        self.extended_stream_id = -1;
                        if flags & 0x01 != 0 && r < self.pes_header_size {
                            /* PES extension */
                            let pes_ext = self.header[r];
                            r += 1;
                            /* Skip PES private data, program packet sequence
                             * counter and P-STD buffer */
                            let mut skip = (pes_ext >> 4) & 0xb;
                            skip += skip & 0x9;
                            r = r.saturating_add(skip as usize);
                            if pes_ext & 0x41 == 0x01 && r + 2 <= self.pes_header_size {
                                /* PES extension 2 */
                                if self.header[r] & 0x7f > 0 && self.header[r + 1] & 0x80 == 0 {
                                    self.extended_stream_id = self.header[r + 1] as i32;
                                }
                            }
                        }

                        /* we got the full header. We parse it and get the payload */
                        self.state = PesState::Payload;
                        self.data_index = 0;
                    }
                }
                PesState::Payload => {
                    let mut max_packet_size = max_packet_size;
                    if self.pes_packet_length != 0
                        && self.pes_packet_length + PES_START_SIZE > self.pes_header_size
                    {
                        let pes_payload_size =
                            self.pes_packet_length + PES_START_SIZE - self.pes_header_size;
                        if pes_payload_size > max_packet_size {
                            self.pes_packet_length = 0;
                        } else {
                            max_packet_size = pes_payload_size;
                        }
                    }

                    let room = max_packet_size.saturating_sub(self.buffer.len());
                    let take = buf.len().min(room);
                    self.buffer.extend_from_slice(&buf[..take]);
                    buf = &buf[take..];

                    if !self.buffer.is_empty() && self.buffer.len() >= max_packet_size {
                        out.push(self.take_packet());
                        self.pes_packet_length = 0;
                        self.stop_parse = true;
                        // FFmpeg's `mpegts_push_data()` sets `buf_size = 0`
                        // after emitting in the payload state: the rest of
                        // this TS packet's payload is stuffing and is dropped.
                        buf = &[];
                    } else if self.pes_packet_length != 0
                        && self.pes_header_size + self.buffer.len()
                            == self.pes_packet_length + PES_START_SIZE
                    {
                        /* emit complete packets with known packet size */
                        out.push(self.take_packet());
                        self.state = PesState::Skip;
                        self.stop_parse = true;
                        buf = &[];
                    }
                }
                PesState::Skip => break,
            }
        }
        out
    }
}

/// `ff_parse_pes_pts()` in FFmpeg `libavformat/mpeg.h`: parse an MPEG-PES
/// five-byte timestamp.
pub fn parse_pes_pts(buf: &[u8]) -> i64 {
    (i64::from(buf[0] & 0x0e) << 29)
        | ((i64::from(u16::from_be_bytes([buf[1], buf[2]])) >> 1) << 15)
        | (i64::from(u16::from_be_bytes([buf[3], buf[4]])) >> 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pes_pts() {
        let b = [0x21, 0x00, 0x05, 0xbf, 0x21]; // PTS = 90000
        assert_eq!(parse_pes_pts(&b), 90000);
        let b = [0x21, 0x00, 0x01, 0x00, 0x05]; // PTS = 2
        assert_eq!(parse_pes_pts(&b), 2);
        let b = [0xff, 0xff, 0xff, 0xff, 0xff]; // 0x1ff_ffff_ffff
        assert_eq!(parse_pes_pts(&b), (1i64 << 33) - 1);
    }

    fn pes_header(stream_id: u8, payload_len: usize, pts: Option<i64>) -> Vec<u8> {
        let mut h = vec![0u8; 9];
        h[0..3].copy_from_slice(&[0, 0, 1]);
        h[3] = stream_id;
        // PES_packet_length counts the bytes after the length field:
        // header[6..9] (3) + optional PTS (5) + payload.
        let total = 8 + payload_len;
        h[4] = (total >> 8) as u8;
        h[5] = total as u8;
        h[6] = 0x80;
        h[7] = if pts.is_some() { 0x80 } else { 0 };
        h[8] = if pts.is_some() { 5 } else { 0 };
        if let Some(ts) = pts {
            let v = ts as u64;
            h.extend_from_slice(&[
                0x21 | ((v >> 29) & 0x0e) as u8,
                ((v >> 22) & 0xff) as u8,
                ((v >> 14) & 0xff) as u8 | 0x01,
                ((v >> 7) & 0xff) as u8,
                ((v & 0x7f) as u8) << 1 | 0x01,
            ]);
        }
        h
    }

    #[test]
    fn assembles_single_packet_pes() {
        let payload = b"hello pes payload";
        let header = pes_header(stream_id::AUDIO_STREAM_0, payload.len(), Some(90000));
        let mut data = header;
        data.extend_from_slice(payload);

        let mut pes = PesContext::new(Some(0));
        assert!(pes.push_data(&data[..10], true, 0, 204800).is_empty());
        let packets = pes.push_data(&data[10..], false, 0, 204800);
        assert_eq!(packets.len(), 1);
        let pkt = &packets[0];
        assert_eq!(pkt.data.data(), payload);
        assert_eq!(pkt.pts, 90000);
        assert_eq!(pkt.dts, 90000);
        assert_eq!(pkt.stream_index, 0);
        assert_eq!(pkt.pos, 0);
        assert_eq!(pkt.side_data[0].data, vec![stream_id::AUDIO_STREAM_0]);
    }

    #[test]
    fn emits_previous_pes_on_new_start_and_continues_parsing() {
        let mut pes = PesContext::new(Some(0));
        let first = {
            let h = pes_header(stream_id::AUDIO_STREAM_0, 4, Some(45000));
            let mut d = h;
            // Unbounded PES so it is only emitted by the next PUSI.
            d[4..6].copy_from_slice(&[0, 0]);
            d.extend_from_slice(b"AAAA");
            d
        };
        assert!(pes.push_data(&first, true, 0, 204800).is_empty());
        let second = {
            let h = pes_header(stream_id::AUDIO_STREAM_0, 4, Some(90000));
            let mut d = h;
            d[4..6].copy_from_slice(&[0, 0]);
            d.extend_from_slice(b"BBBB");
            d
        };
        let packets = pes.push_data(&second, true, 188, 204800);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data.data(), b"AAAA");
        assert_eq!(packets[0].pts, 45000);
        assert!(pes.stop_parse);
        // The remainder of the same TS payload must feed the new PES.
        assert_eq!(pes.state, PesState::Payload);
    }
}
