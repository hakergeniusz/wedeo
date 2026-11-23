//! Constant frame rate enforcement filter.
//!
//! FFmpeg reference: `libavfilter/vf_fps.c`.
//!
//! The filter buffers up to two input frames, converts their timestamps into
//! the output time base (the inverse of the requested frame rate), and emits a
//! copy of a frame at every output timestamp until the next input frame is due.
//! Frames whose interval has already passed by the time the following frame
//! arrives are dropped; frames whose interval spans several output timestamps
//! are duplicated. Frame metadata and side data are carried over because the
//! emitted frames are clones of the input frames.
//!
//! # Interface gap
//!
//! FFmpeg's `activate()` drives the graph and can push the tail (EOF) frames
//! through `ff_filter_frame()`. The frozen [`wedeo_filter::Filter`] trait has
//! `activate(&mut self) -> Result<()>`, which cannot return frames. The tail is
//! therefore produced by [`FpsFilter::finish()`] on the concrete type; it uses
//! the last frame's `duration` as a stand-in for FFmpeg's input status
//! timestamp. An interface change is needed to drive this through the trait.

use std::collections::VecDeque;

use tracing::trace;

use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::{ROUND_PASS_MINMAX, Rational, Rounding, rescale_q_rnd};
use wedeo_core::timestamp::{NOPTS_VALUE, TIME_BASE_Q};
use wedeo_filter::{Filter, FilterDescriptor, FilterPadDescriptor, FilterPadDirection};

// Re-exports so downstream crates whose dependency list does not include
// `wedeo-filter` directly can still discover the registered factory.
pub use wedeo_filter::registry::{FilterFactory, find_filter_by_name};
pub use wedeo_filter::{
    Filter as FilterTrait, FilterDescriptor as FilterDescriptorType, FilterFlags,
};

/// Action performed for the last frame in the FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EofAction {
    /// Round the last frame's end timestamp like every other timestamp.
    Round,
    /// Pass through the last frame: round its end timestamp up.
    Pass,
}

/// A buffered frame, the number of times it has been emitted, and the end
/// timestamp in the *input* time base (used to reproduce FFmpeg's status_pts
/// based tail at end of stream).
#[derive(Debug, Clone)]
struct BufferedFrame {
    frame: Frame,
    out_count: i32,
    end_in: Option<(i64, Rational)>,
}

static INPUT_PADS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];

static OUTPUT_PADS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

fn fps_descriptor() -> &'static FilterDescriptor {
    static DESC: FilterDescriptor = FilterDescriptor {
        name: "fps",
        description: "Force constant framerate.",
        inputs: INPUT_PADS,
        outputs: OUTPUT_PADS,
        flags: FilterFlags::METADATA_ONLY,
    };
    &DESC
}

/// Convert a floating point frame rate into a rational number.
///
/// Uses a continued-fraction expansion with components bounded by `max`,
/// mirroring the intent of FFmpeg's `av_d2q()`. For exact rates prefer
/// [`FpsFilter::new`] / [`FpsFilter::with_fps`].
pub fn f64_to_rational(value: f64, max: i32) -> Rational {
    if value.is_nan() {
        return Rational::new(0, 0);
    }
    if value == 0.0 {
        return Rational::new(0, 1);
    }
    if value.is_infinite() {
        return Rational::new(if value > 0.0 { 1 } else { -1 }, 0);
    }

    let sign = if value < 0.0 { -1i64 } else { 1 };
    let bound = max as i64;
    let mut rem = value.abs();

    // Convergents: h_n = a_n*h_(n-1) + h_(n-2), likewise for k.
    let (mut h_prev2, mut h_prev1) = (0i64, 1i64);
    let (mut k_prev2, mut k_prev1) = (1i64, 0i64);

    loop {
        let a_f = rem.floor();
        if !a_f.is_finite() || a_f > bound as f64 {
            break;
        }
        let a = a_f as i64;
        let h = a * h_prev1 + h_prev2;
        let k = a * k_prev1 + k_prev2;
        if h < 0 || k < 0 || h > bound || k > bound {
            break;
        }
        h_prev2 = h_prev1;
        h_prev1 = h;
        k_prev2 = k_prev1;
        k_prev1 = k;

        let frac = rem - a_f;
        if frac == 0.0 {
            break;
        }
        rem = 1.0 / frac;
        if !rem.is_finite() {
            break;
        }
    }

    if k_prev1 == 0 {
        Rational::new(sign as i32, 0)
    } else {
        Rational::new((sign * h_prev1) as i32, k_prev1 as i32)
    }
}

/// The `fps` filter: convert a variable frame rate stream to a constant one.
pub struct FpsFilter {
    /// Target frame rate (the output link's `frame_rate`).
    rate: Rational,
    /// Output time base (the inverse of `rate`).
    out_time_base: Rational,
    /// Rounding method for timestamp rescaling.
    rounding: Rounding,
    /// Optional start time in seconds.
    start_time: Option<f64>,
    /// Action performed for the buffered last frame.
    eof_action: EofAction,

    /// Input frame pts offset used by `start_time`.
    in_pts_off: i64,
    /// Output frame pts offset used by `start_time`.
    out_pts_off: i64,
    /// Whether the start-time offsets have been computed (needs input tb).
    offsets_computed: bool,

    /// pts of the next frame to write, in output time base.
    next_pts: Option<i64>,
    /// Buffered frames (at most two, matching FFmpeg's `frames[2]`).
    queue: VecDeque<BufferedFrame>,
    /// Whether the end-of-stream tail has already been produced.
    eof_drained: bool,

    // Statistics, matching FFmpeg's FPSContext counters.
    frames_in: i64,
    frames_out: i64,
    dup: i64,
    dropped: i64,
}

impl FpsFilter {
    /// Create a filter with the given target frame rate.
    pub fn new(rate: Rational) -> Result<Self> {
        if rate.num <= 0 || rate.den <= 0 {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            out_time_base: rate.invert(),
            rate,
            rounding: Rounding::NearInf,
            start_time: None,
            eof_action: EofAction::Round,
            in_pts_off: 0,
            out_pts_off: 0,
            offsets_computed: false,
            next_pts: None,
            queue: VecDeque::new(),
            eof_drained: false,
            frames_in: 0,
            frames_out: 0,
            dup: 0,
            dropped: 0,
        })
    }

    /// Create a filter with the FFmpeg default target rate of 25 fps.
    pub fn with_default_rate() -> Result<Self> {
        Self::new(Rational::new(25, 1))
    }

    /// Create a filter with target rate `num / den`.
    pub fn with_fps(num: i32, den: i32) -> Result<Self> {
        Self::new(Rational::new(num, den))
    }

    /// Create a filter with a floating point target rate.
    pub fn with_fps_f64(fps: f64) -> Result<Self> {
        let rate = f64_to_rational(fps, i32::MAX);
        if rate.den == 0 || rate.num <= 0 {
            return Err(Error::InvalidArgument);
        }
        Self::new(rate)
    }

    /// Set the rounding method for timestamp rescaling.
    pub fn rounding(mut self, rounding: Rounding) -> Self {
        self.rounding = rounding;
        self
    }

    /// Assume the first frame's pts corresponds to `start_time` seconds.
    pub fn start_time(mut self, start_time: f64) -> Self {
        self.start_time = Some(start_time);
        self
    }

    /// Set the end-of-stream action.
    pub fn eof_action(mut self, action: EofAction) -> Self {
        self.eof_action = action;
        self
    }

    /// The target frame rate.
    pub fn target_rate(&self) -> Rational {
        self.rate
    }

    /// The output time base (inverse of the target frame rate).
    pub fn output_time_base(&self) -> Rational {
        self.out_time_base
    }

    /// Number of input frames consumed.
    pub fn frames_in(&self) -> i64 {
        self.frames_in
    }

    /// Number of output frames produced (including tail frames).
    pub fn frames_out(&self) -> i64 {
        self.frames_out
    }

    /// Number of frames dropped due to being too close together.
    pub fn dropped(&self) -> i64 {
        self.dropped
    }

    /// Number of duplicated frames produced.
    pub fn duplicated(&self) -> i64 {
        self.dup
    }

    /// Compute the start-time offsets once the input time base is known.
    fn compute_offsets(&mut self, in_tb: Rational) {
        if let Some(start_time) = self.start_time {
            // FFmpeg: double first_pts = s->start_time * AV_TIME_BASE;
            let first_pts = (start_time * wedeo_core::timestamp::TIME_BASE as f64) as i64;
            self.in_pts_off = rescale_q_rnd(
                first_pts,
                TIME_BASE_Q,
                in_tb,
                self.rounding,
                ROUND_PASS_MINMAX != 0,
            );
            self.out_pts_off = rescale_q_rnd(
                first_pts,
                TIME_BASE_Q,
                self.out_time_base,
                self.rounding,
                ROUND_PASS_MINMAX != 0,
            );
            self.next_pts = Some(self.out_pts_off);
            trace!(
                "Set first pts to (in:{} out:{}) from start time {}",
                self.in_pts_off, self.out_pts_off, start_time
            );
        }
    }

    /// Advance the statistics counters when a buffered frame leaves the queue.
    fn record_shift(&mut self, buffered: &BufferedFrame, extra_out: i32) {
        let total = buffered.out_count.saturating_add(extra_out);
        self.frames_out += total as i64;
        if total > 1 {
            self.dup += (total - 1) as i64;
        } else if total == 0 {
            self.dropped += 1;
        }
    }

    /// Feed one input frame into the filter and return every output frame that
    /// can be produced with the frames seen so far.
    fn process_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        if frame.is_audio() {
            return Err(Error::InvalidArgument);
        }

        let in_tb = if frame.time_base.den != 0 && frame.time_base.num != 0 {
            frame.time_base
        } else {
            // No usable time base: assume the pts is already in output units.
            self.out_time_base
        };

        if !self.offsets_computed {
            self.compute_offsets(in_tb);
            self.offsets_computed = true;
        }

        let mut frame = frame;
        let in_pts = frame.pts;

        // Convert frame pts to the output time base. The offsets dance matches
        // FFmpeg's rounding behaviour when the start_time option is used.
        let out_pts = self.out_pts_off.wrapping_add(rescale_q_rnd(
            in_pts.wrapping_sub(self.in_pts_off),
            in_tb,
            self.out_time_base,
            self.rounding,
            true,
        ));

        if self.next_pts.is_none() {
            if out_pts == NOPTS_VALUE {
                trace!("Discarding initial frame(s) with no timestamp.");
                return Ok(Vec::new());
            }
            self.next_pts = Some(out_pts);
            trace!("Set first pts to {}", out_pts);
        }

        let end_in = if frame.duration > 0 {
            Some((in_pts.wrapping_add(frame.duration), in_tb))
        } else {
            None
        };

        frame.pts = out_pts;
        frame.time_base = self.out_time_base;
        self.queue.push_back(BufferedFrame {
            frame,
            out_count: 0,
            end_in,
        });
        self.frames_in += 1;

        let mut outputs = Vec::new();
        // FFmpeg only writes from frames[0] once frames[1] is available: the
        // second frame's pts is the boundary used to decide drop vs duplicate.
        while self.queue.len() >= 2 {
            let second_pts = self.queue[1].frame.pts;
            let next = self.next_pts.unwrap_or(NOPTS_VALUE);

            if second_pts <= next {
                // The second frame is already due: the first one is never
                // displayed and must be dropped.
                let dropped = self.queue.pop_front().expect("queue has two frames");
                self.record_shift(&dropped, 0);
                trace!(
                    "Dropping frame with pts {} (out pts {})",
                    dropped.frame.pts, next
                );
                continue;
            }

            // Output a copy of the first buffered frame at `next`.
            let front = &mut self.queue[0];
            let mut out = front.frame.clone();
            out.pts = next;
            out.duration = 1;
            // av_frame_clone copies pkt_dts/best_effort_timestamp verbatim and
            // only pts/duration are rewritten, so leave those fields intact.
            front.out_count += 1;
            self.next_pts = Some(next.wrapping_add(1));
            trace!("Writing frame with pts {} to pts {}", front.frame.pts, next);
            outputs.push(out);
        }

        Ok(outputs)
    }

    /// Produce the end-of-stream tail frames.
    ///
    /// FFmpeg derives the end timestamp from the input link status (typically
    /// the demuxer's stream end). This interface has no EOF signal, so the last
    /// frame's `duration` is used as a stand-in. Frames with no positive
    /// duration produce no tail. Call this once after the last input frame.
    pub fn finish(&mut self) -> Vec<Frame> {
        if self.eof_drained {
            return Vec::new();
        }
        self.eof_drained = true;

        let mut outputs = Vec::new();
        let front = match self.queue.pop_front() {
            Some(f) => f,
            None => return outputs,
        };
        let next = match self.next_pts {
            Some(n) => n,
            None => return outputs,
        };

        let status_pts = match front.end_in {
            Some((end_in, in_tb)) => {
                let eof_rounding = match self.eof_action {
                    EofAction::Pass => Rounding::Up,
                    EofAction::Round => self.rounding,
                };
                rescale_q_rnd(end_in, in_tb, self.out_time_base, eof_rounding, true)
            }
            None => front.frame.pts,
        };

        let mut n = next;
        while n < status_pts {
            let mut out = front.frame.clone();
            out.pts = n;
            out.duration = 1;
            outputs.push(out);
            n = n.wrapping_add(1);
        }

        self.record_shift(&front, outputs.len() as i32);
        self.next_pts = Some(n);
        trace!(
            "EOF is at pts {} (emitted {} tail frames)",
            status_pts,
            outputs.len()
        );
        outputs
    }
}

impl Filter for FpsFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        fps_descriptor()
    }

    fn activate(&mut self) -> Result<()> {
        // FFmpeg's activate() pumps the graph and can push output frames via
        // ff_filter_frame(). The frozen Filter trait gives activate() no way to
        // return frames, so the tail is exposed through `FpsFilter::finish()`.
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        self.process_frame(frame)
    }
}

/// Factory for the registered `fps` filter.
pub struct FpsFilterFactory {
    rate: Rational,
}

impl FpsFilterFactory {
    /// A factory that creates filters with the given target frame rate.
    pub const fn new(rate: Rational) -> Self {
        Self { rate }
    }
}

impl FilterFactory for FpsFilterFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        fps_descriptor()
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(FpsFilter::new(self.rate)?))
    }
}

static FPS_FACTORY: FpsFilterFactory = FpsFilterFactory::new(Rational::new(25, 1));
inventory::submit!(&FPS_FACTORY as &dyn FilterFactory);

/// True if an `fps` factory is present in the global filter registry.
pub fn is_registered() -> bool {
    find_filter_by_name("fps").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::frame::Frame;
    use wedeo_core::pixel_format::PixelFormat;

    fn video_frame(pts: i64, tb: Rational, duration: i64) -> Frame {
        let mut f = Frame::new_video(16, 16, PixelFormat::Yuv420p);
        f.pts = pts;
        f.time_base = tb;
        f.duration = duration;
        f
    }

    /// Run a list of (pts, duration) frames through the filter and return the
    /// output pts values, in output time base units.
    fn run(in_tb: Rational, rate: Rational, frames: &[(i64, i64)]) -> Vec<i64> {
        let mut filter = FpsFilter::new(rate).unwrap();
        let mut out = Vec::new();
        for &(pts, duration) in frames {
            out.extend(
                filter
                    .process_frame(video_frame(pts, in_tb, duration))
                    .unwrap(),
            );
        }
        out.extend(filter.finish());
        out.into_iter().map(|f| f.pts).collect()
    }

    #[test]
    fn duplicate_slow_input() {
        // FFmpeg: color r=1 d=3 -> fps=10 produces 30 frames, pts 0..29.
        let pts = run(
            Rational::new(1, 1),
            Rational::new(10, 1),
            &[(0, 1), (1, 1), (2, 1)],
        );
        assert_eq!(pts.len(), 30);
        assert_eq!(pts, (0..30).collect::<Vec<_>>());
    }

    #[test]
    fn drop_fast_input() {
        // FFmpeg: color r=10 d=1 (10 frames) -> fps=5 produces pts 0..4.
        let frames: Vec<(i64, i64)> = (0..10).map(|p| (p, 1)).collect();
        let pts = run(Rational::new(1, 10), Rational::new(5, 1), &frames);
        assert_eq!(pts, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn variable_frame_rate() {
        // input frames at 0s, 1s and 3s (tb 1/10), fps=5 -> 20 frames 0..19.
        let pts = run(
            Rational::new(1, 10),
            Rational::new(5, 1),
            &[(0, 10), (10, 10), (30, 10)],
        );
        assert_eq!(pts.len(), 20);
        assert_eq!(pts, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn initial_frames_without_timestamp_are_discarded() {
        let mut filter = FpsFilter::with_fps(25, 1).unwrap();
        let mut no_pts = video_frame(NOPTS_VALUE, Rational::new(1, 25), 1);
        no_pts.pts = NOPTS_VALUE;
        assert!(filter.process_frame(no_pts).unwrap().is_empty());
        let out = filter
            .process_frame(video_frame(0, Rational::new(1, 25), 1))
            .unwrap();
        assert!(out.is_empty()); // need a second frame before writing
        let out = filter
            .process_frame(video_frame(1, Rational::new(1, 25), 1))
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, 0);
    }

    #[test]
    fn metadata_is_preserved() {
        let mut filter = FpsFilter::with_fps(2, 1).unwrap();
        let mut a = video_frame(0, Rational::new(1, 2), 1);
        a.metadata.set("creator", "wedeo");
        let mut b = video_frame(2, Rational::new(1, 2), 1);
        b.metadata.set("creator", "wedeo");

        let mut out = filter.process_frame(a).unwrap();
        out.extend(filter.process_frame(b).unwrap());
        out.extend(filter.finish());
        assert!(!out.is_empty());
        for frame in &out {
            assert_eq!(frame.metadata.get("creator"), Some("wedeo"));
        }
    }

    #[test]
    fn descriptor_and_factory() {
        let filter = FpsFilter::with_default_rate().unwrap();
        let desc = filter.descriptor();
        assert_eq!(desc.name, "fps");
        assert_eq!(desc.description, "Force constant framerate.");
        assert!(desc.flags.contains(FilterFlags::METADATA_ONLY));
        assert_eq!(desc.inputs.len(), 1);
        assert_eq!(desc.outputs.len(), 1);

        let factory = FpsFilterFactory::new(Rational::new(30, 1));
        let created = factory.create().unwrap();
        assert_eq!(created.descriptor().name, "fps");
    }

    #[test]
    fn registration() {
        assert!(is_registered());
        let factory = find_filter_by_name("fps").unwrap();
        assert_eq!(factory.descriptor().name, "fps");
    }

    #[test]
    fn f64_rate_conversion() {
        assert_eq!(f64_to_rational(25.0, i32::MAX), Rational::new(25, 1));
        assert_eq!(f64_to_rational(0.5, i32::MAX), Rational::new(1, 2));
        // 30000/1001 is the canonical NTSC rate.
        let r = f64_to_rational(30000.0 / 1001.0, 100_000);
        assert!((r.to_f64() - 30000.0 / 1001.0).abs() < 1e-4);
    }
}
