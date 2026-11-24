use tracing::debug;
use wedeo_core::error::{Error, Result};
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::Packet;

use crate::demuxer::{Demuxer, ProbeData, SeekFlags, Stream};
use crate::io::{BufferedIo, FileIo};
use crate::muxer::Muxer;
use crate::registry;

/// High-level input context — wraps demuxer + I/O for easy demuxing.
pub struct InputContext {
    demuxer: Box<dyn Demuxer>,
    io: BufferedIo,
    pub streams: Vec<Stream>,
    pub metadata: Metadata,
    pub duration: i64,
    pub start_time: i64,
}

impl InputContext {
    /// Open a file and auto-detect the format.
    pub fn open(path: &str) -> Result<Self> {
        let file_io = FileIo::open(path)?;
        let mut io = BufferedIo::new(Box::new(file_io));

        // Read probe data (up to 4096 bytes, file may be smaller)
        let file_size = io.size().unwrap_or(4096);
        let probe_size = file_size.min(4096) as usize;
        let probe_buf = io.read_bytes(probe_size)?;
        io.seek(0)?;

        let probe_data = ProbeData {
            filename: path,
            buf: &probe_buf,
        };

        let factory = registry::probe(&probe_data).ok_or(Error::DemuxerNotFound)?;

        let mut demuxer = factory.create()?;
        let header = demuxer.read_header(&mut io)?;

        let format_name = factory.descriptor().name;
        debug!(
            path,
            format = format_name,
            streams = header.streams.len(),
            "InputContext opened"
        );

        Ok(Self {
            demuxer,
            io,
            streams: header.streams,
            metadata: header.metadata,
            duration: header.duration,
            start_time: header.start_time,
        })
    }

    /// Read the next packet.
    pub fn read_packet(&mut self) -> Result<Packet> {
        self.demuxer.read_packet(&mut self.io)
    }

    /// Seek to a timestamp in the specified stream.
    pub fn seek(&mut self, stream_index: usize, timestamp: i64, flags: SeekFlags) -> Result<()> {
        self.demuxer
            .seek(&mut self.io, stream_index, timestamp, flags)
    }

    /// Get a stream by index.
    pub fn stream(&self, index: usize) -> Option<&Stream> {
        self.streams.get(index)
    }

    /// Number of streams.
    pub fn nb_streams(&self) -> usize {
        self.streams.len()
    }
}

/// High-level output context — wraps muxer + I/O for easy muxing.
pub struct OutputContext {
    muxer: Box<dyn Muxer>,
    io: BufferedIo,
}

impl OutputContext {
    /// Create an output file with the specified format.
    ///
    /// Equivalent to `create_with_options(path, format_name, streams, &[], &[])`:
    /// no muxer options or metadata are applied.
    pub fn create(path: &str, format_name: &str, streams: &[Stream]) -> Result<Self> {
        Self::create_with_options(path, format_name, streams, &[], &[])
    }

    /// Create an output file, applying muxer options and metadata before the
    /// header is written.
    ///
    /// This is the bridge for CLI flags such as `-movflags +faststart` and
    /// `-metadata key=value`: the pairs are forwarded to the muxer via
    /// [`Muxer::set_option`] / [`Muxer::set_metadata`] before
    /// [`Muxer::write_header`] runs, so the muxer can bake them into the header.
    pub fn create_with_options(
        path: &str,
        format_name: &str,
        streams: &[Stream],
        options: &[(String, String)],
        metadata: &[(String, String)],
    ) -> Result<Self> {
        let factory = registry::find_muxer_by_name(format_name).ok_or(Error::MuxerNotFound)?;
        let muxer = factory.create()?;
        let file_io = FileIo::create(path)?;
        let io = BufferedIo::new(Box::new(file_io));
        Self::create_with_io(muxer, io, streams, options, metadata)
    }

    /// Shared creation path: push options/metadata into the muxer, then write
    /// the header. Kept separate so tests can exercise the ordering without
    /// touching the filesystem or the inventory registry.
    fn create_with_io(
        mut muxer: Box<dyn Muxer>,
        mut io: BufferedIo,
        streams: &[Stream],
        options: &[(String, String)],
        metadata: &[(String, String)],
    ) -> Result<Self> {
        for (key, value) in options {
            muxer.set_option(key, value)?;
        }
        for (key, value) in metadata {
            muxer.set_metadata(key, value)?;
        }
        muxer.write_header(&mut io, streams)?;
        Ok(Self { muxer, io })
    }

    /// Forward a single muxer option to the underlying muxer. Only effective
    /// before the header is written; use [`Self::create_with_options`] to apply
    /// options at creation time.
    pub fn set_option(&mut self, key: &str, value: &str) -> Result<()> {
        self.muxer.set_option(key, value)
    }

    /// Forward a single metadata tag to the underlying muxer. Only effective
    /// before the header is written; use [`Self::create_with_options`] to apply
    /// metadata at creation time.
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        self.muxer.set_metadata(key, value)
    }

    /// Write a single packet.
    pub fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.muxer.write_packet(&mut self.io, packet)
    }

    /// Finalize the output: write trailer and flush.
    pub fn finish(mut self) -> Result<()> {
        self.muxer.write_trailer(&mut self.io)?;
        self.io.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;
    use std::sync::{Arc, Mutex};

    use crate::io::IoContext;

    use super::*;

    /// Minimal seekable in-memory I/O so tests need no filesystem.
    #[derive(Default)]
    struct MemIo {
        buf: Vec<u8>,
        pos: usize,
    }

    impl IoContext for MemIo {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let n = (self.buf.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }

        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            if self.pos + buf.len() > self.buf.len() {
                self.buf.resize(self.pos + buf.len(), 0);
            }
            self.buf[self.pos..self.pos + buf.len()].copy_from_slice(buf);
            self.pos += buf.len();
            Ok(buf.len())
        }

        fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(p) => p as usize,
                SeekFrom::Current(d) => (self.pos as i64 + d) as usize,
                SeekFrom::End(d) => (self.buf.len() as i64 + d) as usize,
            };
            Ok(self.pos as u64)
        }

        fn tell(&mut self) -> Result<u64> {
            Ok(self.pos as u64)
        }

        fn size(&mut self) -> Result<u64> {
            Ok(self.buf.len() as u64)
        }

        fn is_seekable(&self) -> bool {
            true
        }
    }

    /// Muxer that logs every hook call into shared state so the test can still
    /// inspect it after the muxer has been moved into an `OutputContext`.
    struct RecordingMuxer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Muxer for RecordingMuxer {
        fn write_header(&mut self, _io: &mut BufferedIo, _streams: &[Stream]) -> Result<()> {
            self.calls.lock().unwrap().push("header".into());
            Ok(())
        }

        fn write_packet(&mut self, _io: &mut BufferedIo, _packet: &Packet) -> Result<()> {
            Ok(())
        }

        fn write_trailer(&mut self, _io: &mut BufferedIo) -> Result<()> {
            Ok(())
        }

        fn set_option(&mut self, key: &str, value: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("option:{key}={value}"));
            Ok(())
        }

        fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("metadata:{key}={value}"));
            Ok(())
        }
    }

    fn recording_muxer(calls: &Arc<Mutex<Vec<String>>>) -> Box<dyn Muxer> {
        Box::new(RecordingMuxer {
            calls: Arc::clone(calls),
        })
    }

    #[test]
    fn create_with_io_applies_options_and_metadata_before_header() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let muxer = recording_muxer(&calls);
        let io = BufferedIo::new(Box::new(MemIo::default()));
        let options = vec![("movflags".to_string(), "+faststart".to_string())];
        let metadata = vec![("title".to_string(), "Demo".to_string())];

        let _ctx = OutputContext::create_with_io(muxer, io, &[], &options, &metadata).unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "option:movflags=+faststart",
                "metadata:title=Demo",
                "header"
            ]
        );
    }

    #[test]
    fn set_option_and_set_metadata_forward_to_muxer() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let muxer = recording_muxer(&calls);
        let io = BufferedIo::new(Box::new(MemIo::default()));
        let mut ctx = OutputContext { muxer, io };

        ctx.set_option("movflags", "+faststart").unwrap();
        ctx.set_metadata("author", "wedeo").unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["option:movflags=+faststart", "metadata:author=wedeo"]
        );
    }

    #[test]
    fn create_with_options_rejects_unknown_format_without_touching_disk() {
        let err = OutputContext::create_with_options(
            "/tmp/wedeo_ctx_test_unknown_format.mp4",
            "no_such_format_xyz",
            &[],
            &[],
            &[],
        )
        .err()
        .expect("unknown format must fail");
        assert_eq!(err, Error::MuxerNotFound);
    }
}
