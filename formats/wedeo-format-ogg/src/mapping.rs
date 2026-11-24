//! Ogg codec mappings - stream identification, header parsing and per-packet
//! timestamp/duration handling.
//!
//! Ported from FFmpeg's `libavformat/ogg/mapping/{vorbis,opus,theora,flac,ogm}.c`.

use wedeo_codec::decoder::CodecParameters;
use wedeo_core::channel_layout::ChannelLayout;
use wedeo_core::codec_id::CodecId;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::PacketFlags;
use wedeo_core::rational::Rational;

use tracing::debug;
use wedeo_core::error::{Error, Result};

use crate::BitReader;
use crate::vorbis_parser::{
    VORBIS_FLAG_COMMENT, VORBIS_FLAG_HEADER, VORBIS_FLAG_SETUP, VorbisParser,
};

pub const OGG_NOGRANULE_VALUE: u64 = u64::MAX;
pub const NOPTS: i64 = i64::MIN;

/// Codecs recognized by `ogg_find_codec()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecKind {
    Skeleton,
    Dirac,
    Speex,
    Vorbis,
    Theora,
    Flac,
    Opus,
    Vp8,
    OldDirac,
    OldFlac,
    OgmVideo,
    OgmAudio,
    OgmText,
    OgmOld,
}

impl CodecKind {
    /// The byte string used to identify this codec on the BOS page.
    fn magic(self) -> &'static [u8] {
        match self {
            CodecKind::Skeleton => b"fisbone\x00",
            CodecKind::Dirac => b"\x00\x00\x00\x00Dirac",
            CodecKind::Speex => b"Speex   ",
            CodecKind::Vorbis => b"\x01vorbis",
            CodecKind::Theora => b"\x80theora",
            CodecKind::Flac => b"\x7fFLAC",
            CodecKind::Opus => b"OpusHead",
            CodecKind::Vp8 => b"OVP80",
            CodecKind::OldDirac => b"\x00Dirac",
            CodecKind::OldFlac => b"fLaC",
            CodecKind::OgmVideo => b"\x01video",
            CodecKind::OgmAudio => b"\x01audio",
            CodecKind::OgmText => b"\x01text",
            CodecKind::OgmOld => b"\x01Direct Show Samples embedded in Ogg",
        }
    }

    /// Expected number of header packets (`nb_header`).
    pub fn nb_header(self) -> i32 {
        match self {
            CodecKind::Vorbis | CodecKind::Theora => 3,
            CodecKind::Flac | CodecKind::OgmVideo | CodecKind::OgmAudio | CodecKind::OgmText => 2,
            CodecKind::Opus | CodecKind::OgmOld => 1,
            _ => 0,
        }
    }

    /// Whether the granule is the start (rather than the end) timestamp.
    fn granule_is_start(self) -> bool {
        matches!(
            self,
            CodecKind::OgmVideo | CodecKind::OgmAudio | CodecKind::OgmText | CodecKind::OgmOld
        )
    }
}

/// All codecs, in the same order as FFmpeg's `ogg_codecs[]`.
const ALL_CODECS: [CodecKind; 14] = [
    CodecKind::Skeleton,
    CodecKind::Dirac,
    CodecKind::Speex,
    CodecKind::Vorbis,
    CodecKind::Theora,
    CodecKind::Flac,
    CodecKind::Opus,
    CodecKind::Vp8,
    CodecKind::OldDirac,
    CodecKind::OldFlac,
    CodecKind::OgmVideo,
    CodecKind::OgmAudio,
    CodecKind::OgmText,
    CodecKind::OgmOld,
];

/// `ogg_find_codec()`: find the first codec whose magic matches `buf`.
pub fn find_codec(buf: &[u8]) -> Option<CodecKind> {
    ALL_CODECS
        .iter()
        .copied()
        .find(|c| buf.len() >= c.magic().len() && &buf[..c.magic().len()] == c.magic())
}

/// Per-codec private state.
#[derive(Clone, Debug)]
pub enum CodecPrivate {
    None,
    Vorbis(VorbisPrivate),
    Theora(TheoraPrivate),
    Opus(OpusPrivate),
    Flac,
    Ogm,
    Vp8,
}

#[derive(Clone, Debug)]
pub struct VorbisPrivate {
    pub packets: [Option<Vec<u8>>; 3],
    pub parser: Option<VorbisParser>,
    pub ts_offset: i64,
    pub last_page_granule: i64,
    pub bos_page_pos: i64,
    /// Initial timestamp seed used when the demuxer emits no timestamp at all
    /// (e.g. a first data page with granule 0). FFmpeg's probing scan leaves a
    /// doubled pre-skip offset in this case.
    pub seed_pts: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct TheoraPrivate {
    pub gpshift: u32,
    pub gpmask: u64,
    pub version: u32,
    pub packets: [Option<Vec<u8>>; 3],
}

#[derive(Clone, Debug)]
pub struct OpusPrivate {
    pub need_comments: i32,
    pub pre_skip: u32,
    pub cur_dts: i64,
}

/// Complete state of one logical Ogg bitstream.
pub struct OggStream {
    pub serial: u32,
    pub buf: Vec<u8>,
    pub bufpos: usize,
    pub pstart: usize,
    pub psize: usize,
    pub segp: usize,
    pub nsegs: usize,
    pub segments: [u8; 255],
    pub granule: u64,
    pub start_granule: u64,
    pub lastpts: i64,
    pub lastdts: i64,
    pub sync_pos: i64,
    pub page_pos: i64,
    pub flags: u8,
    pub codec: Option<CodecKind>,
    pub header: i32,
    pub nb_header: i32,
    pub incomplete: bool,
    pub page_start: bool,
    pub page_end: bool,
    pub got_data: bool,
    pub pflags: PacketFlags,
    pub pduration: i64,
    pub replace: bool,
    pub start_trimming: u32,
    pub end_trimming: u32,
    pub new_extradata: Option<Vec<u8>>,
    pub params: CodecParameters,
    pub time_base: Rational,
    pub start_time: i64,
    pub duration: i64,
    pub private: CodecPrivate,
    pub metadata: Vec<(String, String)>,
}

impl OggStream {
    pub fn new(serial: u32) -> Self {
        Self {
            serial,
            buf: Vec::new(),
            bufpos: 0,
            pstart: 0,
            psize: 0,
            segp: 0,
            nsegs: 0,
            segments: [0; 255],
            granule: OGG_NOGRANULE_VALUE,
            start_granule: OGG_NOGRANULE_VALUE,
            lastpts: NOPTS,
            lastdts: NOPTS,
            sync_pos: -1,
            page_pos: -1,
            flags: 0,
            codec: None,
            header: -1,
            nb_header: 0,
            incomplete: false,
            page_start: false,
            page_end: false,
            got_data: false,
            pflags: PacketFlags::empty(),
            pduration: 0,
            replace: false,
            start_trimming: 0,
            end_trimming: 0,
            new_extradata: None,
            params: CodecParameters::new(CodecId::None, MediaType::Video),
            time_base: Rational::new(1, 1_000_000),
            start_time: NOPTS,
            duration: NOPTS,
            private: CodecPrivate::None,
            metadata: Vec::new(),
        }
    }

    pub fn packet(&self) -> &[u8] {
        &self.buf[self.pstart..self.pstart + self.psize]
    }

    fn set_pts_info(&mut self, num: i32, den: i32) {
        self.time_base = Rational::new(num, den);
        self.params.time_base = Rational::new(num, den);
    }

    /// `ogg_gptopts()`: translate a granule position into a timestamp.
    pub fn gptopts(&mut self, gp: u64, dts: &mut Option<i64>) -> i64 {
        if gp != OGG_NOGRANULE_VALUE && gp > i64::MAX as u64 {
            return NOPTS;
        }
        if self.codec == Some(CodecKind::Vp8) {
            let invcnt = i64::from((gp >> 30) & 3 == 0);
            let pts = (gp >> 32) as i64 - invcnt;
            let dist = (gp >> 3) & 0x07ff_ffff;
            if dist == 0 {
                self.pflags |= PacketFlags::KEY;
            }
            *dts = Some(pts);
            return pts;
        }
        if self.codec == Some(CodecKind::Theora) {
            if let CodecPrivate::Theora(thp) = &self.private {
                let iframe = gp >> thp.gpshift;
                let pframe = gp & thp.gpmask;
                let mut iframe = iframe as i64;
                if thp.version < 0x0003_0201 {
                    iframe += 1;
                }
                if pframe == 0 {
                    self.pflags |= PacketFlags::KEY;
                }
                let result = iframe + pframe as i64;
                *dts = Some(result);
                return result;
            }
            return NOPTS;
        }
        let pts = gp as i64;
        *dts = Some(pts);
        pts
    }

    /// The granule position is a start timestamp for these codecs.
    pub fn granule_is_start(&self) -> bool {
        self.codec.is_some_and(CodecKind::granule_is_start)
    }

    // -----------------------------------------------------------------------
    // Header processing
    // -----------------------------------------------------------------------

    /// `os->codec->header()`: returns 1 while more headers are expected, 0 when
    /// the packet is a data packet.
    pub fn process_header(&mut self) -> Result<i32> {
        match self.codec {
            Some(CodecKind::Vorbis) => self.vorbis_header(),
            Some(CodecKind::Theora) => self.theora_header(),
            Some(CodecKind::Flac) | Some(CodecKind::OldFlac) => self.flac_header(),
            Some(CodecKind::Opus) => self.opus_header(),
            Some(CodecKind::OgmVideo)
            | Some(CodecKind::OgmAudio)
            | Some(CodecKind::OgmText)
            | Some(CodecKind::OgmOld) => self.ogm_header(),
            Some(CodecKind::Vp8) => self.vp8_header(),
            Some(CodecKind::Skeleton)
            | Some(CodecKind::Dirac)
            | Some(CodecKind::Speex)
            | Some(CodecKind::OldDirac) => {
                // Recognized but not fully mapped: treat the first packet as a
                // header so the demuxer can advance.
                Ok(0)
            }
            _ => Ok(0),
        }
    }

    /// `os->codec->packet()`: fill pflags/pduration/start_trimming/etc. from a
    /// data packet. Returns 1 when the packet should be skipped (in-stream
    /// header), 0 for a regular data packet.
    pub fn process_packet(&mut self) -> Result<i32> {
        match self.codec {
            Some(CodecKind::Vorbis) => self.vorbis_packet(),
            Some(CodecKind::Theora) => self.theora_packet(),
            Some(CodecKind::Flac) | Some(CodecKind::OldFlac) => self.flac_packet(),
            Some(CodecKind::Opus) => self.opus_packet(),
            Some(CodecKind::OgmVideo)
            | Some(CodecKind::OgmAudio)
            | Some(CodecKind::OgmText)
            | Some(CodecKind::OgmOld) => self.ogm_packet(),
            Some(CodecKind::Vp8) => self.vp8_packet(),
            _ => Ok(0),
        }
    }

    // -----------------------------------------------------------------------
    // Vorbis
    // -----------------------------------------------------------------------

    fn vorbis_header(&mut self) -> Result<i32> {
        let pkt_type = self.buf[self.pstart];
        let pkt = self.packet().to_vec();
        debug!(pkt_type, psize = self.psize, "vorbis_header");
        if !self.matches_private(CodecPrivateTag::Vorbis) {
            self.private = CodecPrivate::Vorbis(VorbisPrivate {
                packets: [None, None, None],
                parser: None,
                ts_offset: 0,
                last_page_granule: 0,
                bos_page_pos: -1,
                seed_pts: None,
            });
        }

        if pkt_type & 1 == 0 {
            let has_parser = self.vorbis_parser().is_some();
            return if has_parser {
                Ok(0)
            } else {
                Err(Error::InvalidData)
            };
        }
        if pkt_type > 5 {
            return Ok(1);
        }
        if self.psize < 1 {
            return Err(Error::InvalidData);
        }

        let slot = (pkt_type >> 1) as usize;
        {
            let priv_ = self.private.as_vorbis().ok_or(Error::Bug)?;
            if priv_.packets[slot].is_some() {
                return Err(Error::InvalidData);
            }
            if pkt_type > 1 && priv_.packets[0].is_none()
                || pkt_type > 3 && priv_.packets[1].is_none()
            {
                return if priv_.parser.is_some() {
                    Ok(0)
                } else {
                    Err(Error::InvalidData)
                };
            }
        }
        self.private.as_vorbis_mut().ok_or(Error::Bug)?.packets[slot] = Some(pkt.clone());

        if pkt_type == 1 {
            return self.vorbis_parse_header();
        }
        if pkt_type == 3 {
            self.parse_vorbis_comment(&pkt[7..]);
            return Ok(1);
        }

        // Setup header: pack the three headers into Xiph-laced extradata and
        // initialize the duration parser.
        let extradata =
            Self::pack_xiph_headers(&self.private.as_vorbis().ok_or(Error::Bug)?.packets)?;
        let parser = match VorbisParser::new(&extradata) {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    ?e,
                    extradata_len = extradata.len(),
                    "vorbis parser init failed"
                );
                self.params.extradata.clear();
                return Err(Error::InvalidData);
            }
        };
        self.params.extradata = extradata;
        {
            let p = self.private.as_vorbis_mut().ok_or(Error::Bug)?;
            p.parser = Some(parser);
            // fixup_vorbis_headers() frees the three source packets, so the
            // chained-header re-init in vorbis_packet() does not trigger for
            // the initial stream.
            p.packets = [None, None, None];
        }
        Ok(1)
    }

    fn vorbis_parse_header(&mut self) -> Result<i32> {
        let p = self.packet().to_vec();
        if p.len() != 30 {
            return Err(Error::InvalidData);
        }
        let version = u32::from_le_bytes([p[7], p[8], p[9], p[10]]);
        if version != 0 {
            return Err(Error::InvalidData);
        }
        let channels = p[11];
        let srate = u32::from_le_bytes([p[12], p[13], p[14], p[15]]);
        let bitrate = u32::from_le_bytes([p[20], p[21], p[22], p[23]]);
        let blocksize = p[28];
        let bs0 = blocksize & 0x0f;
        let bs1 = blocksize >> 4;
        if bs0 > bs1 || bs0 < 6 || bs1 > 13 {
            return Err(Error::InvalidData);
        }
        if p[29] != 1 {
            return Err(Error::InvalidData);
        }

        self.params.codec_id = CodecId::Vorbis;
        self.params.media_type = MediaType::Audio;
        self.params.channel_layout = ChannelLayout::unspec(channels as i32);
        self.params.bit_rate = bitrate as i64;
        if srate > 0 {
            self.params.sample_rate = srate;
            self.set_pts_info(1, srate as i32);
        }
        Ok(1)
    }

    fn vorbis_packet(&mut self) -> Result<i32> {
        if self.psize == 0 {
            return Err(Error::InvalidData);
        }
        if self.vorbis_parser().is_none() {
            return Err(Error::InvalidData);
        }

        if let CodecPrivate::Vorbis(priv_) = &mut self.private
            && priv_.bos_page_pos < 0
        {
            priv_.bos_page_pos = self.page_pos;
        }
        let bos = self.page_start
            && self
                .private
                .as_vorbis()
                .is_some_and(|p| p.bos_page_pos == self.page_pos);
        if (bos || (self.page_start && self.lastpts == NOPTS))
            && let Some(p) = self.private.as_vorbis_mut()
            && let Some(parser) = &mut p.parser
        {
            parser.reset();
        }
        if bos && let Some(p) = self.private.as_vorbis_mut() {
            p.last_page_granule = 0;
        }

        let pkt = self.packet().to_vec();
        let mut flags = 0u32;
        let duration = {
            let parser = self.private.as_vorbis_mut().and_then(|p| p.parser.as_mut());
            match parser {
                Some(parser) => parser.frame_flags(&pkt, &mut flags)?,
                None => return Err(Error::InvalidData),
            }
        };
        if duration < 0 {
            self.pflags |= PacketFlags::CORRUPT;
            return Ok(0);
        }
        self.pduration = duration;

        // ts_offset from the very first chained stream.
        let ts_offset = if let Some(p) = self.private.as_vorbis_mut() {
            if p.ts_offset == 0 && self.pduration != 0 {
                p.ts_offset = self.pduration;
            }
            p.ts_offset
        } else {
            0
        };

        if bos {
            if self.lastpts != NOPTS {
                if self.start_time == NOPTS {
                    self.start_time = self.lastpts;
                    if self.duration != NOPTS {
                        self.duration -= self.start_time;
                    }
                }
                self.lastpts -= ts_offset;
            }
            if self.lastdts != NOPTS {
                self.lastdts -= ts_offset;
            }
        }
        // FFmpeg's probing/length scan primes the private parser state so that
        // the first data packet also derives its timestamp from the page
        // granule; do the same by running this for BOS packets too.
        if self.lastpts == NOPTS && self.flags & 4 == 0 && self.granule != OGG_NOGRANULE_VALUE {
            // Compute the duration of every packet on this page, then derive
            // the first packet's timestamp from the page granule.
            let mut page_duration = self.pduration;
            let mut last_idx = self.pstart + self.psize;
            let mut next_idx = last_idx;
            for &seg in &self.segments[self.segp..self.nsegs] {
                next_idx += seg as usize;
                if seg < 255 && next_idx != last_idx {
                    let d = {
                        let parser = self.private.as_vorbis_mut().and_then(|p| p.parser.as_mut());
                        match parser {
                            Some(parser) if last_idx < self.buf.len() => {
                                parser.frame(&self.buf[last_idx..last_idx + 1])
                            }
                            _ => Err(Error::InvalidData),
                        }
                    };
                    match d {
                        Ok(d) if d >= 0 => page_duration += d,
                        _ => {
                            page_duration = self.granule as i64;
                            break;
                        }
                    }
                    last_idx = next_idx;
                }
            }
            // Re-run on the current packet so the parser's window state is
            // consistent for the following page.
            if let Some(parser) = self.private.as_vorbis_mut().and_then(|p| p.parser.as_mut()) {
                let mut f = 0;
                let _ = parser.frame_flags(&pkt, &mut f);
            }
            self.lastpts = self.granule as i64 - page_duration;
            self.lastdts = self.lastpts;
        }

        if self.granule == 0 && self.pduration != 0 {
            if let Some(p) = self.private.as_vorbis_mut()
                && p.seed_pts.is_none()
            {
                p.seed_pts = Some(-2 * p.ts_offset);
            }
            self.lastpts = NOPTS;
            self.lastdts = NOPTS;
        }

        let mut skip_packet = false;
        if flags & VORBIS_FLAG_HEADER != 0 {
            let chained_header = self.packet().to_vec();
            if let Some(p) = self.private.as_vorbis_mut() {
                p.packets = [None, None, None];
            }
            self.vorbis_parse_header()?;
            self.private.as_vorbis_mut().ok_or(Error::Bug)?.packets[0] = Some(chained_header);
            skip_packet = true;
        }
        if flags & VORBIS_FLAG_COMMENT != 0 {
            let comment = self.packet()[7..].to_vec();
            self.parse_vorbis_comment(&comment);
            let pkt = self.packet().to_vec();
            self.private.as_vorbis_mut().ok_or(Error::Bug)?.packets[1] = Some(pkt);
            skip_packet = true;
            flags = 0;
        }
        if flags & VORBIS_FLAG_SETUP != 0 {
            let pkt = self.packet().to_vec();
            self.private.as_vorbis_mut().ok_or(Error::Bug)?.packets[2] = Some(pkt);
            skip_packet = true;
        }

        // All three headers of a chained stream have been collected: pack
        // them as new extradata and re-initialize the duration parser.
        let complete = self.private.as_vorbis().is_some_and(|p| {
            p.packets[0].is_some() && p.packets[1].is_some() && p.packets[2].is_some()
        });
        if complete {
            let extradata =
                Self::pack_xiph_headers(&self.private.as_vorbis().ok_or(Error::Bug)?.packets)?;
            if let Ok(parser) = VorbisParser::new(&extradata) {
                self.new_extradata = Some(extradata);
                if let Some(p) = self.private.as_vorbis_mut() {
                    p.parser = Some(parser);
                    p.bos_page_pos = -1;
                }
            }
        }

        // EOS end-trimming.
        if self.flags & 4 != 0 {
            let last_page_granule = self
                .private
                .as_vorbis()
                .map(|p| p.last_page_granule)
                .unwrap_or(0);
            if self.page_start
                && let Some(p) = self.private.as_vorbis_mut()
            {
                p.last_page_granule = 0;
            }
            if self.segp != self.nsegs {
                if let Some(p) = self.private.as_vorbis_mut() {
                    p.last_page_granule += self.pduration;
                }
            } else {
                let skip = self
                    .private
                    .as_vorbis()
                    .map(|p| p.last_page_granule)
                    .unwrap_or(0)
                    + self.pduration
                    - (self.granule as i64 - last_page_granule);
                if skip > 0 {
                    self.end_trimming = skip as u32;
                }
            }
        } else if let Some(p) = self.private.as_vorbis_mut() {
            p.last_page_granule = self.granule as i64;
        }

        Ok(if skip_packet { 1 } else { 0 })
    }

    // -----------------------------------------------------------------------
    // Opus
    // -----------------------------------------------------------------------

    fn opus_header(&mut self) -> Result<i32> {
        if !self.matches_private(CodecPrivateTag::Opus) {
            self.private = CodecPrivate::Opus(OpusPrivate {
                need_comments: 0,
                pre_skip: 0,
                cur_dts: 0,
            });
        }
        if self.flags & 2 != 0 {
            return self.parse_opus_header();
        }
        let need_comments = self.private.as_opus().map(|p| p.need_comments).unwrap_or(0);
        if need_comments > 0 {
            let p = self.packet().to_vec();
            if p.len() < 8 || &p[..8] != b"OpusTags" {
                return Err(Error::InvalidData);
            }
            self.parse_vorbis_comment(&p[8..]);
            if let Some(priv_) = self.private.as_opus_mut() {
                priv_.need_comments -= 1;
            }
            return Ok(1);
        }
        Ok(0)
    }

    fn parse_opus_header(&mut self) -> Result<i32> {
        let p = self.packet().to_vec();
        if p.len() < 19 || p[8] & 0xf0 != 0 {
            return Err(Error::InvalidData);
        }
        let channels = p[9];
        let pre_skip = u16::from_le_bytes([p[10], p[11]]) as u32;
        self.params.codec_id = CodecId::Opus;
        self.params.media_type = MediaType::Audio;
        self.params.channel_layout = ChannelLayout::unspec(channels as i32);
        self.params.sample_rate = 48_000;
        self.params.extradata = p.to_vec();
        self.set_pts_info(1, 48_000);
        self.start_trimming = pre_skip;
        if let Some(priv_) = self.private.as_opus_mut() {
            priv_.pre_skip = pre_skip;
            priv_.need_comments = 1;
            // FFmpeg's length scan seeds `cur_dts` with the negative pre-skip
            // (the private state is not restored by ogg_restore()).
            priv_.cur_dts = -(pre_skip as i64);
        }
        Ok(1)
    }

    fn opus_packet(&mut self) -> Result<i32> {
        if self.psize == 0 {
            return Err(Error::InvalidData);
        }
        if self.granule > (1u64 << 62) {
            return Err(Error::InvalidData);
        }
        let p = self.packet().to_vec();
        if p.len() > 8 && &p[..8] == b"OpusHead" {
            self.parse_opus_header()?;
            self.new_extradata = Some(p.to_vec());
            return Ok(1);
        }
        if p.len() > 8 && &p[..8] == b"OpusTags" {
            self.parse_vorbis_comment(&p[8..]);
            if let Some(priv_) = self.private.as_opus_mut() {
                priv_.need_comments = 0;
            }
            return Ok(1);
        }

        if (self.lastpts == 0 || self.lastpts == NOPTS) && self.flags & 4 == 0 {
            let mut duration = 0i64;
            let mut seg = self.segp;
            let d = opus_duration(&p)?;
            if d < 0 {
                self.pflags |= PacketFlags::CORRUPT;
                return Ok(0);
            }
            duration += d;
            let mut last_idx = self.pstart + self.psize;
            let mut next_idx = last_idx;
            while seg < self.nsegs {
                next_idx += self.segments[seg] as usize;
                if self.segments[seg] < 255 && next_idx != last_idx {
                    let d = if last_idx < self.buf.len() {
                        opus_duration(&self.buf[last_idx..next_idx])?
                    } else {
                        0
                    };
                    if d > 0 {
                        duration += d;
                    }
                    last_idx = next_idx;
                }
                seg += 1;
            }
            self.lastpts = self.granule as i64 - duration;
            self.lastdts = self.lastpts;
        }

        let duration = opus_duration(&p)?;
        self.pduration = duration;
        if self.lastpts != NOPTS {
            if self.start_time == NOPTS {
                self.start_time = self.lastpts;
            }
            let pre_skip = self.private.as_opus().map(|x| x.pre_skip).unwrap_or(0) as i64;
            self.lastpts -= pre_skip;
            self.lastdts = self.lastpts;
            if let Some(priv_) = self.private.as_opus_mut() {
                priv_.cur_dts = self.lastpts;
            }
        }
        if let Some(priv_) = self.private.as_opus_mut() {
            priv_.cur_dts += self.pduration;
        }
        if self.flags & 4 != 0 {
            let pre_skip = self.private.as_opus().map(|x| x.pre_skip).unwrap_or(0) as i64;
            let cur_dts = self.private.as_opus().map(|x| x.cur_dts).unwrap_or(0);
            let mut skip = cur_dts - self.granule as i64 + pre_skip;
            if skip > self.pduration {
                skip = self.pduration;
            }
            if skip > 0 {
                self.pduration = if skip < self.pduration {
                    self.pduration - skip
                } else {
                    1
                };
                self.end_trimming = skip as u32;
            }
            // FFmpeg also sets AV_PKT_FLAG_EOF here; wedeo's PacketFlags has no
            // equivalent, so the flag is deliberately not set.
        }
        Ok(0)
    }

    // -----------------------------------------------------------------------
    // Theora
    // -----------------------------------------------------------------------

    fn theora_header(&mut self) -> Result<i32> {
        let first = self.buf[self.pstart];
        if first & 0x80 == 0 {
            return Ok(0);
        }
        let type_ = (first & 0x7f) as usize;
        if !self.matches_private(CodecPrivateTag::Theora) {
            self.private = CodecPrivate::Theora(TheoraPrivate {
                gpshift: 0,
                gpmask: 0,
                version: 0,
                packets: [None, None, None],
            });
        }

        if type_ == 0 {
            self.theora_parse_id()?;
        } else if type_ == 1 {
            let comment = self.packet()[7..].to_vec();
            self.parse_vorbis_comment(&comment);
        } else if type_ == 2 {
            let version = self.private.as_theora().map(|p| p.version).unwrap_or(0);
            if version == 0 {
                return Err(Error::InvalidData);
            }
        } else {
            return Err(Error::InvalidData);
        }

        let pkt = self.packet().to_vec();
        self.private.as_theora_mut().ok_or(Error::Bug)?.packets[type_] = Some(pkt);
        if type_ == 2 {
            let extradata =
                Self::pack_xiph_headers(&self.private.as_theora().ok_or(Error::Bug)?.packets)?;
            self.params.extradata = extradata;
        }
        Ok(1)
    }

    fn theora_parse_id(&mut self) -> Result<()> {
        let p = self.packet().to_vec();
        let mut gb = BitReader::new(&p);
        gb.skip(7 * 8); // 0x80"theora"
        let version = gb.bits(24);
        if version < 0x0003_0100 {
            return Err(Error::InvalidData);
        }
        let mut width = (gb.bits(16) << 4) as i64;
        let mut height = (gb.bits(16) << 4) as i64;
        if version >= 0x0003_0400 {
            gb.skip(100);
        }
        if version >= 0x0003_0200 {
            let w = gb.bits(24) as i64;
            let h = gb.bits(24) as i64;
            if w <= width && w > width - 16 && h <= height && h > height - 16 {
                width = w;
                height = h;
            }
            gb.skip(16);
        }
        let den = gb.bits(32);
        let num = gb.bits(32);
        let (num, den) = if num > 0 && den > 0 {
            (num, den)
        } else {
            (1, 25)
        };
        let _sar_num = gb.bits(24);
        let _sar_den = gb.bits(24);
        if version >= 0x0003_0200 {
            gb.skip(38);
        }
        if version >= 0x0030_4000 {
            gb.skip(2);
        }
        let gpshift = gb.bits(5);
        let gpmask = (1u64 << gpshift) - 1;

        self.params.codec_id = CodecId::Theora;
        self.params.media_type = MediaType::Video;
        self.params.width = width.max(0) as u32;
        self.params.height = height.max(0) as u32;
        self.set_pts_info(num as i32, den as i32);
        if let CodecPrivate::Theora(thp) = &mut self.private {
            thp.version = version;
            thp.gpshift = gpshift;
            thp.gpmask = gpmask;
        }
        Ok(())
    }

    fn theora_packet(&mut self) -> Result<i32> {
        let type_ = {
            let t = if self.psize > 0 {
                self.buf[self.pstart]
            } else {
                0
            };
            if t & 0x80 == 0 {
                0u8
            } else if t <= 0x82 {
                1 + (t & 0x7f)
            } else {
                4
            }
        };
        // 0 = DATA, 1 = ID, 2 = COMMENT, 3 = SETUP, 4 = RESERVED
        if (1..=3).contains(&type_) {
            let slot = (type_ - 1) as usize;
            let pkt = self.packet().to_vec();
            self.private.as_theora_mut().ok_or(Error::Bug)?.packets[slot] = Some(pkt);
            if type_ == 2 && self.psize > 7 {
                // Metadata for the chained stream.
            }
            let complete = self.private.as_theora().is_some_and(|p| {
                p.packets[0].is_some() && p.packets[1].is_some() && p.packets[2].is_some()
            });
            if complete {
                let extradata =
                    Self::pack_xiph_headers(&self.private.as_theora().ok_or(Error::Bug)?.packets)?;
                self.new_extradata = Some(extradata);
            }
            return Ok(1);
        }

        if (self.lastpts == 0 || self.lastpts == NOPTS) && self.flags & 4 == 0 {
            let mut duration = 1i64;
            for &seg in &self.segments[self.segp..self.nsegs] {
                if seg < 255 {
                    duration += 1;
                }
            }
            let mut pts = self.gptopts(self.granule, &mut None);
            if pts != NOPTS {
                pts -= duration;
            }
            self.lastpts = pts;
            self.lastdts = pts;
            if self.start_time == NOPTS && self.lastpts != NOPTS {
                self.start_time = self.lastpts;
                if self.duration > 0 {
                    self.duration -= self.start_time;
                }
            }
        }
        self.pduration = 1;
        Ok(0)
    }

    // -----------------------------------------------------------------------
    // FLAC
    // -----------------------------------------------------------------------

    fn flac_header(&mut self) -> Result<i32> {
        let p = self.packet().to_vec();
        if p.is_empty() {
            return Err(Error::InvalidData);
        }
        if p[0] == 0xff {
            return Ok(0);
        }
        let mdt = p[0] & 0x7f;
        if mdt == 0x7f {
            if p.len() < 17 + 34 {
                return Err(Error::InvalidData);
            }
            // Packets start with "\177FLAC"; mdt is the first byte & 0x7f.
            // Skip "FLAC" (bytes 1..5), major version (byte 5), minor version
            // and header count (bytes 6..9) and "fLaC" (bytes 9..13).
            if &p[1..5] != b"FLAC" || p[5] != 1 || &p[9..13] != b"fLaC" {
                return Err(Error::InvalidData);
            }
            let off = 13;
            if u32::from_be_bytes([p[off], p[off + 1], p[off + 2], p[off + 3]]) != 34 {
                return Err(Error::InvalidData);
            }
            let streaminfo = &p[off + 4..off + 4 + 34];
            let samplerate =
                (u32::from_be_bytes([0, streaminfo[10], streaminfo[11], streaminfo[12]]) >> 4)
                    as i32;
            debug!(samplerate, len = p.len(), "flac STREAMINFO");
            if samplerate == 0 {
                return Err(Error::InvalidData);
            }
            self.params.codec_id = CodecId::Flac;
            self.params.media_type = MediaType::Audio;
            self.params.sample_rate = samplerate as u32;
            self.params.extradata = streaminfo.to_vec();
            self.set_pts_info(1, samplerate);
        } else if mdt == 4 {
            self.parse_vorbis_comment(&p[4..]);
        }
        if !self.matches_private(CodecPrivateTag::Flac) {
            self.private = CodecPrivate::Flac;
        }
        Ok(1)
    }

    fn flac_packet(&mut self) -> Result<i32> {
        let p = self.packet().to_vec();
        if p.len() > 5 && &p[..5] == b"\x7fFLAC" {
            const OFFSET: usize = 5 + 1 + 1 + 2 + 4 + 4;
            if p.len() >= OFFSET + 34 {
                self.new_extradata = Some(p[OFFSET..OFFSET + 34].to_vec());
            }
            return Ok(1);
        }
        if !p.is_empty() && p[0] & 0x7f == 4 {
            self.parse_vorbis_comment(&p[4..]);
            return Ok(1);
        }
        if !p.is_empty() {
            self.pduration = flac_frame_duration(&p);
        }
        Ok(0)
    }

    // -----------------------------------------------------------------------
    // Ogm
    // -----------------------------------------------------------------------

    fn ogm_header(&mut self) -> Result<i32> {
        let p = self.packet().to_vec();
        if p.is_empty() {
            return Err(Error::InvalidData);
        }
        if p[0] & 1 == 0 {
            return Ok(0);
        }
        if p[0] == 3 {
            if p.len() > 8 {
                self.parse_vorbis_comment(&p[7..p.len() - 1]);
            }
            return Ok(1);
        }
        if p[0] != 1 {
            return Ok(1);
        }
        // The full DirectShow-style header parsing is only used by unusual
        // files; the common "\x01video"/"\x01audio"/"\x01text" layout is
        // implemented here.
        let mut pos = 1usize;
        if p.get(pos) == Some(&b'v') {
            self.params.media_type = MediaType::Video;
            pos += 8;
            if pos + 4 > p.len() {
                return Err(Error::InvalidData);
            }
            let tag = u32::from_le_bytes([p[pos], p[pos + 1], p[pos + 2], p[pos + 3]]);
            self.params.codec_tag = tag;
            self.params.codec_id = fourcc_to_codec_id(tag, true);
            pos += 4;
        } else if p.get(pos) == Some(&b't') {
            self.params.media_type = MediaType::Subtitle;
            self.params.codec_id = CodecId::SubText;
            pos += 12;
        } else {
            self.params.media_type = MediaType::Audio;
            pos += 8;
            if pos + 4 > p.len() {
                return Err(Error::InvalidData);
            }
            let cid = u32::from_str_radix(std::str::from_utf8(&p[pos..pos + 4]).unwrap_or("0"), 16)
                .unwrap_or(0);
            self.params.codec_id = fourcc_to_codec_id(cid, false);
            pos += 4;
        }

        if pos + 20 > p.len() {
            return Err(Error::InvalidData);
        }
        let size = u32::from_le_bytes([p[pos], p[pos + 1], p[pos + 2], p[pos + 3]]) as usize;
        let time_unit = u64::from_le_bytes([
            p[pos + 4],
            p[pos + 5],
            p[pos + 6],
            p[pos + 7],
            p[pos + 8],
            p[pos + 9],
            p[pos + 10],
            p[pos + 11],
        ]);
        let spu = u64::from_le_bytes([
            p[pos + 12],
            p[pos + 13],
            p[pos + 14],
            p[pos + 15],
            p[pos + 16],
            p[pos + 17],
            p[pos + 18],
            p[pos + 19],
        ]);
        if time_unit == 0 || spu == 0 {
            return Err(Error::InvalidData);
        }
        pos += 20 + 4 + 8; // default_len, buffersize + bits_per_sample

        if self.params.media_type == MediaType::Video {
            if pos + 8 > p.len() {
                return Err(Error::InvalidData);
            }
            self.params.width = u32::from_le_bytes([p[pos], p[pos + 1], p[pos + 2], p[pos + 3]]);
            self.params.height =
                u32::from_le_bytes([p[pos + 4], p[pos + 5], p[pos + 6], p[pos + 7]]);
            self.set_pts_info(time_unit as i32, (spu * 10_000_000) as i32);
        } else {
            if pos + 8 > p.len() {
                return Err(Error::InvalidData);
            }
            let channels = u16::from_le_bytes([p[pos], p[pos + 1]]);
            self.params.channel_layout = ChannelLayout::unspec(channels as i32);
            let bitrate = u32::from_le_bytes([p[pos + 4], p[pos + 5], p[pos + 6], p[pos + 7]]);
            self.params.bit_rate = bitrate as i64 * 8;
            let sample_rate = (spu * 10_000_000 / time_unit) as i32;
            self.params.sample_rate = sample_rate as u32;
            self.set_pts_info(1, sample_rate);
        }
        let _ = size;
        if !self.matches_private(CodecPrivateTag::Ogm) {
            self.private = CodecPrivate::Ogm;
        }
        Ok(1)
    }

    fn ogm_packet(&mut self) -> Result<i32> {
        if self.psize == 0 {
            return Err(Error::InvalidData);
        }
        let first = self.buf[self.pstart];
        if first & 8 != 0 {
            self.pflags |= PacketFlags::KEY;
        }
        let lb = (((first & 2) << 1) | ((first >> 6) & 3)) as usize;
        if self.psize < lb + 1 {
            return Err(Error::InvalidData);
        }
        self.pstart += lb + 1;
        self.psize -= lb + 1;
        for i in (0..lb).rev() {
            self.pduration += (self.buf[self.pstart + 1 + i] as i64) << (i * 8);
        }
        Ok(0)
    }

    // -----------------------------------------------------------------------
    // VP8 (OggVP8 mapping)
    // -----------------------------------------------------------------------

    fn vp8_header(&mut self) -> Result<i32> {
        let p = self.packet().to_vec();
        if p.len() < 7 || p[0] != 0x4f {
            return Ok(0);
        }
        match p[5] {
            0x01 => {
                if p.len() < 26 || p[6] != 1 {
                    return Err(Error::InvalidData);
                }
                let width = u16::from_be_bytes([p[8], p[9]]);
                let height = u16::from_be_bytes([p[10], p[11]]);
                let den = u32::from_be_bytes([p[18], p[19], p[20], p[21]]);
                let num = u32::from_be_bytes([p[22], p[23], p[24], p[25]]);
                self.params.codec_id = CodecId::Vp8;
                self.params.media_type = MediaType::Video;
                self.params.width = width as u32;
                self.params.height = height as u32;
                if num > 0 && den > 0 {
                    self.set_pts_info(den as i32, num as i32);
                }
            }
            0x02 => {
                if p.len() > 7 && p[6] == 0x20 {
                    self.parse_vorbis_comment(&p[7..]);
                }
            }
            _ => return Err(Error::InvalidData),
        }
        if !matches!(self.private, CodecPrivate::Vp8) {
            self.private = CodecPrivate::Vp8;
        }
        Ok(1)
    }

    fn vp8_packet(&mut self) -> Result<i32> {
        if self.psize == 0 {
            return Ok(0);
        }
        if (self.lastpts == 0 || self.lastpts == NOPTS) && self.flags & 4 == 0 {
            let mut duration = 0i64;
            if self.pstart < self.buf.len() {
                duration += i64::from((self.buf[self.pstart] >> 4) & 1);
            }
            let mut last_idx = self.pstart + self.psize;
            let mut next_idx = last_idx;
            for &seg in &self.segments[self.segp..self.nsegs] {
                if seg < 255 {
                    if last_idx < self.buf.len() {
                        duration += i64::from((self.buf[last_idx] >> 4) & 1);
                    }
                    last_idx = next_idx + seg as usize;
                }
                next_idx += seg as usize;
            }
            let gp = self.granule;
            let mut dts = None;
            let pts = self.gptopts(gp, &mut dts);
            self.lastpts = pts - duration;
            self.lastdts = self.lastpts;
            if self.start_time == NOPTS && self.lastpts != NOPTS {
                self.start_time = self.lastpts;
            }
        }
        self.pduration = i64::from((self.buf[self.pstart] >> 4) & 1);
        Ok(0)
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn pack_xiph_headers(packets: &[Option<Vec<u8>>; 3]) -> Result<Vec<u8>> {
        let lens = [
            packets[0].as_ref().map_or(0, Vec::len),
            packets[1].as_ref().map_or(0, Vec::len),
            packets[2].as_ref().map_or(0, Vec::len),
        ];
        let total = lens[0] as u64 + lens[1] as u64 + lens[2] as u64;
        if total + total / 255 + 64 > i32::MAX as u64 {
            return Err(Error::InvalidData);
        }
        let mut out = Vec::with_capacity((total + total / 255 + 64) as usize);
        out.push(2);
        for &len in &lens[..2] {
            let mut len = len;
            while len >= 255 {
                out.push(255);
                len -= 255;
            }
            out.push(len as u8);
        }
        for p in packets.iter().flatten() {
            out.extend_from_slice(p);
        }
        Ok(out)
    }

    fn parse_vorbis_comment(&mut self, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut pos = 4usize.min(data.len());
        let end = (4 + len).min(data.len());
        if pos < end {
            let vendor = String::from_utf8_lossy(&data[pos..end]).into_owned();
            self.metadata.push(("encoder".to_string(), vendor));
            pos = end;
        }
        if pos + 4 > data.len() {
            return;
        }
        let count =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        for _ in 0..count {
            if pos + 4 > data.len() {
                break;
            }
            let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            if pos + l > data.len() {
                break;
            }
            let entry = String::from_utf8_lossy(&data[pos..pos + l]).into_owned();
            pos += l;
            if let Some((key, value)) = entry.split_once('=') {
                self.metadata
                    .push((key.to_ascii_lowercase(), value.to_string()));
            }
        }
    }

    fn matches_private(&self, tag: CodecPrivateTag) -> bool {
        matches!(
            (tag, &self.private),
            (CodecPrivateTag::Vorbis, CodecPrivate::Vorbis(_))
                | (CodecPrivateTag::Theora, CodecPrivate::Theora(_))
                | (CodecPrivateTag::Opus, CodecPrivate::Opus(_))
                | (CodecPrivateTag::Flac, CodecPrivate::Flac)
                | (CodecPrivateTag::Ogm, CodecPrivate::Ogm)
        )
    }

    fn vorbis_parser(&self) -> Option<&VorbisParser> {
        match &self.private {
            CodecPrivate::Vorbis(p) => p.parser.as_ref(),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum CodecPrivateTag {
    Vorbis,
    Theora,
    Opus,
    Flac,
    Ogm,
}

impl CodecPrivate {
    pub(crate) fn as_vorbis(&self) -> Option<&VorbisPrivate> {
        if let CodecPrivate::Vorbis(p) = self {
            Some(p)
        } else {
            None
        }
    }
    fn as_vorbis_mut(&mut self) -> Option<&mut VorbisPrivate> {
        if let CodecPrivate::Vorbis(p) = self {
            Some(p)
        } else {
            None
        }
    }
    pub(crate) fn as_opus(&self) -> Option<&OpusPrivate> {
        if let CodecPrivate::Opus(p) = self {
            Some(p)
        } else {
            None
        }
    }
    fn as_opus_mut(&mut self) -> Option<&mut OpusPrivate> {
        if let CodecPrivate::Opus(p) = self {
            Some(p)
        } else {
            None
        }
    }
    fn as_theora(&self) -> Option<&TheoraPrivate> {
        if let CodecPrivate::Theora(p) = self {
            Some(p)
        } else {
            None
        }
    }
    fn as_theora_mut(&mut self) -> Option<&mut TheoraPrivate> {
        if let CodecPrivate::Theora(p) = self {
            Some(p)
        } else {
            None
        }
    }
}

/// `opus_duration()`: decode the packet duration from the TOC byte.
fn opus_duration(src: &[u8]) -> Result<i64> {
    if src.is_empty() {
        return Err(Error::InvalidData);
    }
    let mut nb_frames = 1i64;
    let toc = src[0] as u32;
    let toc_config = toc >> 3;
    let toc_count = toc & 3;
    let frame_size = if toc_config < 12 {
        480.max(960 * (toc_config & 3)) as i64
    } else if toc_config < 16 {
        480i64 << (toc_config & 1)
    } else {
        120i64 << (toc_config & 3)
    };
    if toc_count == 3 {
        if src.len() < 2 {
            return Err(Error::InvalidData);
        }
        nb_frames = (src[1] & 0x3f) as i64;
    } else if toc_count != 0 {
        nb_frames = 2;
    }
    Ok(frame_size * nb_frames)
}

/// Extract the block size (hence the duration in samples) from a FLAC frame
/// header.  Equivalent to `ff_flac_decode_frame_header()`'s blocksize parsing
/// in `libavcodec/flac.c`.
fn flac_frame_duration(p: &[u8]) -> i64 {
    const BLOCKSIZE: [i64; 16] = [
        0, 192, 576, 1152, 2304, 4608, 0, 0, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
    ];
    let mut gb = BitReader::new(p);
    if gb.bits(15) & 0x7fff != 0x7ffc {
        return 0;
    }
    let _is_var_size = gb.bit();
    let bs_code = gb.bits(4) as usize;
    let _sr_code = gb.bits(4);
    let _ch_mode = gb.bits(4);
    let _bps_code = gb.bits(3);
    let _reserved = gb.bit();

    // FFmpeg's GET_UTF8 macro reads 8 bits at a time from the bitstream.
    let mut val = gb.bits(8);
    let mut top = (val & 128) >> 1;
    if val & 0xc0 == 0x80 || val >= 0xfe {
        return 0;
    }
    while val & top != 0 {
        let tmp = gb.bits(8) as i32 - 128;
        if tmp >> 6 != 0 {
            return 0;
        }
        val = (val << 6).wrapping_add(tmp as u32);
        top <<= 5;
    }

    match bs_code {
        0 => 0,
        6 => gb.bits(8) as i64 + 1,
        7 => gb.bits(16) as i64 + 1,
        _ => BLOCKSIZE[bs_code],
    }
}

/// Map a 32-bit codec tag to a CodecId (subset of FFmpeg's BMP/WAV tag lists).
fn fourcc_to_codec_id(tag: u32, bmp: bool) -> CodecId {
    let fourcc = tag.to_le_bytes();
    if bmp {
        match &fourcc {
            b"MP4V" | b"mp4v" => CodecId::Mpeg4,
            b"DIVX" | b"divx" => CodecId::Mpeg4,
            b"XVID" | b"xvid" => CodecId::Mpeg4,
            b"H264" | b"h264" => CodecId::H264,
            b"VP80" | b"vp80" => CodecId::Vp8,
            _ => CodecId::None,
        }
    } else {
        match &fourcc {
            b"0001" => CodecId::PcmS16le,
            b"0003" => CodecId::PcmF32le,
            b"FFFE" => CodecId::Mp3,
            b"00FF" => CodecId::Aac,
            _ => CodecId::None,
        }
    }
}
