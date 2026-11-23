//! Static tables from FFmpeg `libavcodec/flacdata.c` and `flac.c`.

use wedeo_core::channel_layout::{Channel, ChannelLayout};

/// `ff_flac_sample_rate_table` from flacdata.c.
pub const SAMPLE_RATE_TABLE: [u32; 16] = [
    0, 88200, 176400, 192000, 8000, 16000, 22050, 24000, 32000, 44100, 48000, 96000, 0, 0, 0, 0,
];

/// `ff_flac_blocksize_table` from flacdata.c.
pub const BLOCKSIZE_TABLE: [u32; 16] = [
    0, 192, 576, 1152, 2304, 4608, 0, 0, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
];

/// `sample_size_table` from flac.c.
pub const SAMPLE_SIZE_TABLE: [u32; 8] = [0, 8, 12, 0, 16, 20, 24, 32];

/// `flac_channel_layouts` from flac.c (FFmpeg's standard layouts for 1..8
/// channels).
pub fn flac_channel_layout(channels: u32) -> ChannelLayout {
    let channels_vec = match channels {
        1 => vec![Channel::FrontCenter],
        2 => vec![Channel::FrontLeft, Channel::FrontRight],
        3 => vec![
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::FrontCenter,
        ],
        4 => vec![
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::BackLeft,
            Channel::BackRight,
        ],
        5 => vec![
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::FrontCenter,
            Channel::BackLeft,
            Channel::BackRight,
        ],
        6 => vec![
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::FrontCenter,
            Channel::LowFrequency,
            Channel::BackLeft,
            Channel::BackRight,
        ],
        7 => vec![
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::FrontCenter,
            Channel::LowFrequency,
            Channel::BackLeft,
            Channel::BackRight,
            Channel::BackCenter,
        ],
        8 => vec![
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::FrontCenter,
            Channel::LowFrequency,
            Channel::BackLeft,
            Channel::BackRight,
            Channel::SideLeft,
            Channel::SideRight,
        ],
        _ => return ChannelLayout::unspec(channels as i32),
    };
    ChannelLayout {
        order: wedeo_core::channel_layout::ChannelOrder::Native,
        nb_channels: channels as i32,
        channels: channels_vec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_match_ffmpeg() {
        assert_eq!(SAMPLE_RATE_TABLE[9], 44100);
        assert_eq!(SAMPLE_RATE_TABLE[11], 96000);
        assert_eq!(BLOCKSIZE_TABLE[8], 256);
        assert_eq!(BLOCKSIZE_TABLE[15], 32768);
        assert_eq!(SAMPLE_SIZE_TABLE, [0, 8, 12, 0, 16, 20, 24, 32]);
    }

    #[test]
    fn layouts() {
        assert_eq!(flac_channel_layout(1).nb_channels, 1);
        assert_eq!(flac_channel_layout(2).channels.len(), 2);
        assert_eq!(flac_channel_layout(6).channels[3], Channel::LowFrequency);
        assert_eq!(flac_channel_layout(9).nb_channels, 9);
    }
}
