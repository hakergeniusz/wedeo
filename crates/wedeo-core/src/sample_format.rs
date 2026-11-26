use std::fmt;

/// Audio sample format, matching FFmpeg's AVSampleFormat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum SampleFormat {
    None = -1,
    /// Unsigned 8 bits.
    U8 = 0,
    /// Signed 16 bits.
    S16 = 1,
    /// Signed 32 bits.
    S32 = 2,
    /// Float.
    Flt = 3,
    /// Double.
    Dbl = 4,
    /// Unsigned 8 bits, planar.
    U8p = 5,
    /// Signed 16 bits, planar.
    S16p = 6,
    /// Signed 32 bits, planar.
    S32p = 7,
    /// Float, planar.
    Fltp = 8,
    /// Double, planar.
    Dblp = 9,
    /// Signed 64 bits.
    S64 = 10,
    /// Signed 64 bits, planar.
    S64p = 11,
}

impl SampleFormat {
    /// Return the number of bytes per sample.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::None => 0,
            SampleFormat::U8 | SampleFormat::U8p => 1,
            SampleFormat::S16 | SampleFormat::S16p => 2,
            SampleFormat::S32 | SampleFormat::S32p | SampleFormat::Flt | SampleFormat::Fltp => 4,
            SampleFormat::Dbl | SampleFormat::Dblp | SampleFormat::S64 | SampleFormat::S64p => 8,
        }
    }

    /// Check if the sample format is planar.
    pub fn is_planar(self) -> bool {
        matches!(
            self,
            SampleFormat::U8p
                | SampleFormat::S16p
                | SampleFormat::S32p
                | SampleFormat::Fltp
                | SampleFormat::Dblp
                | SampleFormat::S64p
        )
    }

    /// Get the packed equivalent of a planar format (or return self if already packed).
    pub fn packed(self) -> Self {
        match self {
            SampleFormat::U8p => SampleFormat::U8,
            SampleFormat::S16p => SampleFormat::S16,
            SampleFormat::S32p => SampleFormat::S32,
            SampleFormat::Fltp => SampleFormat::Flt,
            SampleFormat::Dblp => SampleFormat::Dbl,
            SampleFormat::S64p => SampleFormat::S64,
            other => other,
        }
    }

    /// Get the planar equivalent of a packed format (or return self if already planar).
    pub fn planar(self) -> Self {
        match self {
            SampleFormat::U8 => SampleFormat::U8p,
            SampleFormat::S16 => SampleFormat::S16p,
            SampleFormat::S32 => SampleFormat::S32p,
            SampleFormat::Flt => SampleFormat::Fltp,
            SampleFormat::Dbl => SampleFormat::Dblp,
            SampleFormat::S64 => SampleFormat::S64p,
            other => other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::None => "none",
            SampleFormat::U8 => "u8",
            SampleFormat::S16 => "s16",
            SampleFormat::S32 => "s32",
            SampleFormat::Flt => "flt",
            SampleFormat::Dbl => "dbl",
            SampleFormat::U8p => "u8p",
            SampleFormat::S16p => "s16p",
            SampleFormat::S32p => "s32p",
            SampleFormat::Fltp => "fltp",
            SampleFormat::Dblp => "dblp",
            SampleFormat::S64 => "s64",
            SampleFormat::S64p => "s64p",
        }
    }
}

impl fmt::Display for SampleFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Read packed sample `i` as a sign-extended integer (U8 is treated as S8).
fn read_int(data: &[u8], fmt: SampleFormat, i: usize) -> i128 {
    let b = fmt.bytes_per_sample();
    let c = &data[i * b..(i + 1) * b];
    match fmt {
        SampleFormat::U8 => c[0] as i8 as i128,
        SampleFormat::S16 => i16::from_ne_bytes([c[0], c[1]]) as i128,
        SampleFormat::S32 => i32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as i128,
        SampleFormat::S64 => {
            i64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as i128
        }
        _ => 0,
    }
}

/// Read packed sample `i` as f64 (integer formats are normalized to [-1, 1)).
fn read_float(data: &[u8], fmt: SampleFormat, i: usize) -> f64 {
    let b = fmt.bytes_per_sample();
    let c = &data[i * b..(i + 1) * b];
    match fmt {
        SampleFormat::U8 => (c[0] as i8 as f64) / 128.0,
        SampleFormat::S16 => i16::from_ne_bytes([c[0], c[1]]) as f64 / 32768.0,
        SampleFormat::S32 => i32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as f64 / 2147483648.0,
        SampleFormat::S64 => {
            i64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as f64
                / 9223372036854775808.0
        }
        SampleFormat::Flt => f32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as f64,
        SampleFormat::Dbl => f64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]),
        _ => 0.0,
    }
}

fn write_int(v: i128, fmt: SampleFormat, out: &mut Vec<u8>) {
    match fmt {
        SampleFormat::U8 => out.push((v + 128).clamp(0, 255) as u8),
        SampleFormat::S16 => out
            .extend_from_slice(&(v.clamp(i16::MIN as i128, i16::MAX as i128) as i16).to_ne_bytes()),
        SampleFormat::S32 => out
            .extend_from_slice(&(v.clamp(i32::MIN as i128, i32::MAX as i128) as i32).to_ne_bytes()),
        SampleFormat::S64 => out.extend_from_slice(&(v as i64).to_ne_bytes()),
        _ => {}
    }
}

fn write_float(v: f64, fmt: SampleFormat, out: &mut Vec<u8>) {
    match fmt {
        SampleFormat::U8 => out.push((v * 128.0 + 128.0).round().clamp(0.0, 255.0) as u8),
        SampleFormat::S16 => out.extend_from_slice(
            &((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_ne_bytes(),
        ),
        SampleFormat::S32 => out.extend_from_slice(
            &((v * 2147483648.0)
                .round()
                .clamp(-2147483648.0, 2147483647.0) as i32)
                .to_ne_bytes(),
        ),
        SampleFormat::S64 => {
            let x = (v * 9223372036854775808.0).round();
            out.extend_from_slice(
                &(x.clamp(-9223372036854775808.0, 9223372036854775807.0) as i64).to_ne_bytes(),
            );
        }
        SampleFormat::Flt => out.extend_from_slice(&(v as f32).to_ne_bytes()),
        SampleFormat::Dbl => out.extend_from_slice(&v.to_ne_bytes()),
        _ => {}
    }
}

fn is_int(fmt: SampleFormat) -> bool {
    matches!(
        fmt,
        SampleFormat::U8 | SampleFormat::S16 | SampleFormat::S32 | SampleFormat::S64
    )
}

fn is_float(fmt: SampleFormat) -> bool {
    matches!(fmt, SampleFormat::Flt | SampleFormat::Dbl)
}

/// Convert packed interleaved samples between formats, matching FFmpeg's
/// swresample semantics (no dithering): integer-to-integer conversions shift
/// (truncate) like `swr`, float conversions round-to-nearest and clip.
///
/// `from`/`to` must be packed formats; use [`SampleFormat::packed`] first if
/// the frame may be planar. Returns the input unchanged when the formats
/// already match.
pub fn convert_samples(data: &[u8], from: SampleFormat, to: SampleFormat) -> Vec<u8> {
    let from = from.packed();
    let to = to.packed();
    if from == to || from == SampleFormat::None || to == SampleFormat::None {
        return data.to_vec();
    }
    let b_in = from.bytes_per_sample();
    let b_out = to.bytes_per_sample();
    if b_in == 0 || b_out == 0 {
        return Vec::new();
    }
    let n = data.len() / b_in;
    let mut out = Vec::with_capacity(n * b_out);
    let int_to_int = is_int(from) && is_int(to);
    // Bit depth of the integer formats (U8 counts as 8-bit signed).
    let shift_of = |f: SampleFormat| -> i32 {
        match f {
            SampleFormat::U8 => 8,
            SampleFormat::S16 => 16,
            SampleFormat::S32 => 32,
            SampleFormat::S64 => 64,
            _ => 0,
        }
    };
    for i in 0..n {
        if int_to_int {
            let v = read_int(data, from, i);
            let shift = shift_of(from) - shift_of(to);
            let v = if shift > 0 { v >> shift } else { v << (-shift) };
            write_int(v, to, &mut out);
        } else if is_float(to) {
            write_float(read_float(data, from, i), to, &mut out);
        } else {
            // Float source into integer target: scale, round, clip (swr style).
            write_float(read_float(data, from, i), to, &mut out);
        }
    }
    out
}
