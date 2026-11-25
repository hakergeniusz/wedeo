//! FATE test support library.
//!
//! Provides utilities for running FATE-style comparison tests between
//! wedeo and FFmpeg outputs.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Location of the FATE sample suite (set via env or default).
pub fn fate_suite_dir() -> PathBuf {
    std::env::var("FATE_SUITE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("fate-suite"))
}

/// Check if a FATE sample file exists.
pub fn fate_sample(relative_path: &str) -> Option<PathBuf> {
    let path = fate_suite_dir().join(relative_path);
    if path.exists() { Some(path) } else { None }
}

/// Return early from a test when zero optional FATE samples were checked.
///
/// Tests that `continue` past missing samples must not fail just because the
/// FATE suite was not installed (CI has no samples). When at least one sample
/// was checked, all comparisons still run and still fail normally.
#[macro_export]
macro_rules! skip_if_no_samples {
    ($checked:expr, $what:literal) => {
        if $checked == 0 {
            eprintln!(
                "SKIP: no {} FATE samples available (set FATE_SUITE to enable this test)",
                $what
            );
            return;
        }
    };
}

/// Compute MD5 of a file.
pub fn md5_file(path: &Path) -> std::io::Result<String> {
    let data = std::fs::read(path)?;
    Ok(md5_bytes(&data))
}

/// Compute MD5 of raw bytes.
pub fn md5_bytes(data: &[u8]) -> String {
    use md5::Digest;
    let result = md5::Md5::digest(data);
    result
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Compare two text outputs line by line, ignoring trailing whitespace.
/// Returns Ok(()) if they match, Err with diff info if not.
pub fn diff_output(expected: &str, actual: &str) -> Result<(), String> {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();

    if expected_lines.len() != actual_lines.len() {
        return Err(format!(
            "Line count differs: expected {} lines, got {} lines",
            expected_lines.len(),
            actual_lines.len()
        ));
    }

    for (i, (exp, act)) in expected_lines.iter().zip(actual_lines.iter()).enumerate() {
        let exp_trimmed = exp.trim_end();
        let act_trimmed = act.trim_end();
        if exp_trimmed != act_trimmed {
            return Err(format!(
                "Line {} differs:\n  expected: {}\n  actual:   {}",
                i + 1,
                exp_trimmed,
                act_trimmed
            ));
        }
    }

    Ok(())
}

/// Run wedeo-framecrc on a file and capture stdout.
pub fn run_framecrc(input_path: &Path, binary: &str) -> Result<String, String> {
    let output = Command::new(binary)
        .arg(input_path.to_str().unwrap())
        .output()
        .map_err(|e| format!("Failed to run wedeo-framecrc: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "wedeo-framecrc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run wedeo-audiogen to create a test WAV file.
pub fn run_audiogen(
    output_path: &Path,
    sample_rate: u32,
    channels: u32,
    binary: &str,
) -> Result<(), String> {
    let output = Command::new(binary)
        .arg(output_path.to_str().unwrap())
        .arg(sample_rate.to_string())
        .arg(channels.to_string())
        .output()
        .map_err(|e| format!("Failed to run wedeo-audiogen: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "wedeo-audiogen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

/// Standard Adler-32 (s1=1 init, per RFC 1950).
pub fn adler32_compute(data: &[u8]) -> u32 {
    let mut hasher = adler2::Adler32::new();
    hasher.write_slice(data);
    hasher.checksum()
}

/// Run wedeo-framecrc binary on a file and return stdout lines.
pub fn run_wedeo_framecrc(path: &Path) -> Result<Vec<String>, String> {
    let bin = std::env::var("CARGO_BIN_EXE_wedeo-framecrc")
        .or_else(|_| {
            // Fallback: search in target directory relative to manifest
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let workspace = manifest_dir.parent().unwrap().parent().unwrap();
            for profile in &["debug", "release"] {
                let bin = workspace
                    .join("target")
                    .join(profile)
                    .join("wedeo-framecrc");
                if bin.exists() {
                    return Ok(bin.to_str().unwrap().to_string());
                }
            }
            Err(std::env::VarError::NotPresent)
        })
        .map_err(|_| "wedeo-framecrc binary not found".to_string())?;

    let output = Command::new(&bin)
        .arg(path.to_str().unwrap())
        .output()
        .map_err(|e| format!("Failed to run wedeo-framecrc: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "wedeo-framecrc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|l| l.to_string()).collect())
}

/// Run FFmpeg in framecrc decode mode for a video file.
/// Returns None if ffmpeg is not available.
pub fn run_ffmpeg_framecrc_video(path: &Path) -> Option<Vec<String>> {
    let output = Command::new("ffmpeg")
        .args([
            "-bitexact",
            "-i",
            path.to_str().unwrap(),
            "-f",
            "framecrc",
            "-",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(stdout.lines().map(|l| l.to_string()).collect())
}

/// A comma-separated field in an FFmpeg framecrc data line.
///
/// FFmpeg's `framecrc_write_packet()` (libavformat/framecrcenc.c:74) emits:
///
/// ```text
/// stream_index, dts, pts, duration, size, 0x<adler32>[, F=0x..][, S=.. ...]
/// ```
///
/// Header lines (`#tb`, `#media_type`, `#codec_id`, `#dimensions`, `#sar`, ...) come
/// from `ff_framehash_write_header()` (libavformat/framehash.c:25) and are ignored
/// by the comparison helpers in this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramecrcField {
    StreamIndex,
    Dts,
    Pts,
    Duration,
    Size,
    Checksum,
    Flags,
    /// Side-data count/name/size/checksum fields (`S=...` and subsequent triples).
    Extra(usize),
}

impl FramecrcField {
    /// Presentation timestamps describe ordering, not decoded pixel identity.
    pub fn is_timestamp(self) -> bool {
        matches!(self, Self::Dts | Self::Pts | Self::Duration)
    }
}

/// Explicit tolerance policy for full-line framecrc comparison.
///
/// # Comparison policy
///
/// * Size and checksum are always exact: they are the bit-exact conformance
///   contract (Adler-32 over the decoded frame bytes, FFmpeg's `s1 = s2 = 0`).
/// * Stream index and packet flags are exact by default; they encode which
///   stream and whether the packet is a key frame.
/// * DTS/PTS/duration differences are collected as *notes* by default. FFmpeg's
///   framecrc prints the container timestamps verbatim, while wedeo's decode
///   path may report presentation timestamps for both dts and pts. Timestamp
///   drift must never mask nor manufacture a decoded-pixel mismatch, so it is
///   reported separately instead of failing the comparison.
///
/// Use [`FramecrcPolicy::default()`] for the standard policy, or override the
/// booleans for a stricter (all-exact) comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramecrcPolicy {
    pub exact_stream_index: bool,
    pub exact_size: bool,
    pub exact_checksum: bool,
    pub exact_flags: bool,
    pub timestamps_are_notes: bool,
}

impl Default for FramecrcPolicy {
    fn default() -> Self {
        Self {
            exact_stream_index: true,
            exact_size: true,
            exact_checksum: true,
            exact_flags: true,
            timestamps_are_notes: true,
        }
    }
}

/// One field-level difference between a wedeo and an FFmpeg framecrc data line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FramecrcFieldDiff {
    /// Zero-based index among data lines (header lines are excluded).
    pub frame: usize,
    pub field: FramecrcField,
    pub wedeo: String,
    pub ffmpeg: String,
}

/// Differences split into hard failures and diagnostic notes by a [`FramecrcPolicy`].
#[derive(Clone, Debug, Default)]
pub struct FramecrcVerdict {
    /// Field differences that make the comparison fail.
    pub mismatches: Vec<FramecrcFieldDiff>,
    /// Timestamp differences, reported for diagnostics only under the default policy.
    pub notes: Vec<FramecrcFieldDiff>,
}

impl FramecrcVerdict {
    /// True when no hard mismatch was found (notes are allowed).
    pub fn is_pass(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// Human-readable report of mismatches and timestamp notes.
    pub fn format(&self) -> String {
        let mut out = String::new();
        for diff in &self.mismatches {
            out.push_str(&format!(
                "Frame {} {:?} mismatch:\n  wedeo:  {}\n  ffmpeg: {}\n",
                diff.frame, diff.field, diff.wedeo, diff.ffmpeg
            ));
        }
        for diff in &self.notes {
            out.push_str(&format!(
                "Frame {} {:?} note (tolerance):\n  wedeo:  {}\n  ffmpeg: {}\n",
                diff.frame, diff.field, diff.wedeo, diff.ffmpeg
            ));
        }
        out.trim_end().to_string()
    }
}

/// Policy-independent result of a full field-by-field framecrc comparison.
#[derive(Clone, Debug, Default)]
pub struct FramecrcComparison {
    /// Structural problems (malformed lines, differing frame counts).
    /// These are always hard errors, independent of the tolerance policy.
    pub structural_errors: Vec<String>,
    pub differences: Vec<FramecrcFieldDiff>,
    pub wedeo_frame_count: usize,
    pub ffmpeg_frame_count: usize,
}

impl FramecrcComparison {
    /// True only when both outputs have the same data-line count and every field matches.
    pub fn is_exact(&self) -> bool {
        self.structural_errors.is_empty()
            && self.differences.is_empty()
            && self.wedeo_frame_count == self.ffmpeg_frame_count
    }

    /// Apply a tolerance policy, splitting differences into mismatches and notes.
    pub fn evaluate(&self, policy: &FramecrcPolicy) -> FramecrcVerdict {
        let mut verdict = FramecrcVerdict::default();
        for diff in &self.differences {
            let is_note = diff.field.is_timestamp() && policy.timestamps_are_notes;
            let is_exact = match diff.field {
                FramecrcField::StreamIndex => policy.exact_stream_index,
                FramecrcField::Size => policy.exact_size,
                FramecrcField::Checksum => policy.exact_checksum,
                FramecrcField::Flags => policy.exact_flags,
                // Extra side-data fields have no dedicated tolerance knob; they
                // are always exact because they change the checksummed payload.
                FramecrcField::Extra(_) => true,
                FramecrcField::Dts | FramecrcField::Pts | FramecrcField::Duration => !is_note,
            };
            if is_exact {
                verdict.mismatches.push(diff.clone());
            } else {
                verdict.notes.push(diff.clone());
            }
        }
        verdict
    }

    /// Convert into a `Result` using the supplied policy.
    pub fn into_result(self, policy: &FramecrcPolicy) -> Result<(), String> {
        if !self.structural_errors.is_empty() {
            return Err(self.structural_errors.join("\n"));
        }
        let verdict = self.evaluate(policy);
        if verdict.is_pass() {
            Ok(())
        } else {
            Err(verdict.format())
        }
    }
}

/// Compare full framecrc lines field by field, ignoring comment/header lines.
///
/// This is the structured counterpart to [`compare_framecrc_lines`]: it records
/// every differing field so callers can apply their own [`FramecrcPolicy`] and
/// distinguish timestamp drift from bit-exact data mismatches.
pub fn compare_framecrc_lines_full(wedeo: &[String], ffmpeg: &[String]) -> FramecrcComparison {
    fn data_lines(lines: &[String]) -> Vec<&str> {
        lines
            .iter()
            .map(String::as_str)
            .filter(|l| !l.starts_with('#'))
            .collect()
    }
    let wedeo_data = data_lines(wedeo);
    let ffmpeg_data = data_lines(ffmpeg);

    let mut result = FramecrcComparison {
        wedeo_frame_count: wedeo_data.len(),
        ffmpeg_frame_count: ffmpeg_data.len(),
        ..FramecrcComparison::default()
    };

    if wedeo_data.len() != ffmpeg_data.len() {
        result.structural_errors.push(format!(
            "Frame count differs: wedeo={}, ffmpeg={}",
            wedeo_data.len(),
            ffmpeg_data.len()
        ));
    }

    for (i, (w, f)) in wedeo_data.iter().zip(ffmpeg_data.iter()).enumerate() {
        let w_fields: Vec<&str> = w.split(',').map(str::trim).collect();
        let f_fields: Vec<&str> = f.split(',').map(str::trim).collect();

        if w_fields.len() < 6 || f_fields.len() < 6 {
            result.structural_errors.push(format!(
                "Frame {i}: malformed framecrc line\n  wedeo:  {w}\n  ffmpeg: {f}"
            ));
            continue;
        }

        let field_for = |index: usize, value: &str| -> FramecrcField {
            match index {
                0 => FramecrcField::StreamIndex,
                1 => FramecrcField::Dts,
                2 => FramecrcField::Pts,
                3 => FramecrcField::Duration,
                4 => FramecrcField::Size,
                5 => FramecrcField::Checksum,
                _ if value.starts_with("F=") => FramecrcField::Flags,
                _ => FramecrcField::Extra(index - 6),
            }
        };
        let field_count = w_fields.len().max(f_fields.len());
        for index in 0..field_count {
            let wv = w_fields.get(index).copied().unwrap_or("");
            let fv = f_fields.get(index).copied().unwrap_or("");
            if wv == fv {
                continue;
            }
            result.differences.push(FramecrcFieldDiff {
                frame: i,
                field: field_for(index, if wv.is_empty() { fv } else { wv }),
                wedeo: wv.to_string(),
                ffmpeg: fv.to_string(),
            });
        }
    }

    result
}

/// Compare framecrc lines between wedeo and FFmpeg, ignoring comment lines.
/// Returns Ok(()) if all data lines match on size and checksum fields.
///
/// Timestamp differences are reported by [`compare_framecrc_lines_full`] but do
/// not fail this legacy helper; see [`FramecrcPolicy::default()`].
pub fn compare_framecrc_lines(wedeo: &[String], ffmpeg: &[String]) -> Result<(), String> {
    compare_framecrc_lines_full(wedeo, ffmpeg).into_result(&FramecrcPolicy::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_md5() {
        assert_eq!(md5_bytes(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            md5_bytes(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn test_adler32() {
        // "Wikipedia" -> 0x11E60398
        let checksum = super::adler32_compute(b"Wikipedia");
        assert_eq!(checksum, 0x11E60398);
    }

    fn line(ts: &str, size: &str, crc: &str) -> String {
        format!("0, {ts}, {ts}, 1, {size}, {crc}")
    }

    #[test]
    fn test_framecrc_field_comparison_exact_and_policy() {
        let wedeo = vec!["#tb 0: 1/25".to_string(), line("0", "152064", "0x12345678")];
        let ffmpeg = vec![
            "#tb 0: 1/25".to_string(),
            line("7", "152064", "0x12345678"), // only timestamps differ
        ];
        let cmp = compare_framecrc_lines_full(&wedeo, &ffmpeg);
        assert!(!cmp.is_exact());
        assert_eq!(cmp.differences.len(), 2); // dts, pts
        assert!(cmp.differences.iter().all(|d| d.field.is_timestamp()));

        // Default policy: timestamps are notes, so the legacy helper still passes.
        assert!(compare_framecrc_lines(&wedeo, &ffmpeg).is_ok());
        let verdict = cmp.evaluate(&FramecrcPolicy::default());
        assert!(verdict.is_pass());
        assert!(verdict.mismatches.is_empty());
        assert_eq!(verdict.notes.len(), 2);
        assert!(verdict.format().contains("note (tolerance)"));

        // All-exact policy: timestamp drift becomes a hard failure.
        let strict = FramecrcPolicy {
            timestamps_are_notes: false,
            ..FramecrcPolicy::default()
        };
        let strict_verdict = cmp.evaluate(&strict);
        assert!(!strict_verdict.is_pass());
        assert_eq!(strict_verdict.mismatches.len(), 2);
        assert!(cmp.clone().into_result(&strict).is_err());
    }

    #[test]
    fn test_framecrc_field_comparison_size_and_checksum_are_always_exact() {
        let wedeo = vec![line("0", "152064", "0x12345678")];
        let ffmpeg = vec![line("0", "152064", "0xdeadbeef")];
        let cmp = compare_framecrc_lines_full(&wedeo, &ffmpeg);
        assert_eq!(
            cmp.differences,
            vec![FramecrcFieldDiff {
                frame: 0,
                field: FramecrcField::Checksum,
                wedeo: "0x12345678".to_string(),
                ffmpeg: "0xdeadbeef".to_string(),
            }]
        );
        assert!(compare_framecrc_lines(&wedeo, &ffmpeg).is_err());

        let wedeo = vec![line("0", "152064", "0x12345678")];
        let ffmpeg = vec![line("0", "152068", "0x12345678")];
        let cmp = compare_framecrc_lines_full(&wedeo, &ffmpeg);
        assert_eq!(cmp.differences[0].field, FramecrcField::Size);
        assert!(compare_framecrc_lines(&wedeo, &ffmpeg).is_err());
    }

    #[test]
    fn test_framecrc_flags_and_structural_errors() {
        let wedeo = vec![format!("{}, F=0x0001", line("0", "10", "0x1"))];
        let ffmpeg = vec![line("0", "10", "0x1")];
        let cmp = compare_framecrc_lines_full(&wedeo, &ffmpeg);
        assert_eq!(cmp.differences[0].field, FramecrcField::Flags);
        assert!(compare_framecrc_lines(&wedeo, &ffmpeg).is_err());

        // Relaxing flags is explicit, not implicit.
        let relaxed = FramecrcPolicy {
            exact_flags: false,
            ..FramecrcPolicy::default()
        };
        assert!(cmp.clone().into_result(&relaxed).is_ok());

        let err = compare_framecrc_lines(&[line("0", "10", "0x1")], &[]).unwrap_err();
        assert_eq!(err, "Frame count differs: wedeo=1, ffmpeg=0");

        let err =
            compare_framecrc_lines(&["garbage".to_string()], &["junk".to_string()]).unwrap_err();
        assert!(err.contains("malformed framecrc line"));
    }
}
