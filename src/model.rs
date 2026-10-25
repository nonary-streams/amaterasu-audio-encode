use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;
pub const MAX_PACKETS: usize = 24;
pub type EncodeResult<T> = Result<T, &'static str>;

#[derive(Clone, Copy)]
pub struct ChannelLayout {
    pub streams: i32,
    pub coupled_streams: i32,
    /// RFC 7845 family-1 mapping in Vorbis speaker order for surround.
    pub mapping: &'static [u8],
    /// Decoder mapping that instead returns the documented WAVE speaker order.
    pub wave_mapping: &'static [u8],
    /// For each Vorbis input channel, its index in interleaved WAVE PCM.
    pub vorbis_from_wave: &'static [usize],
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Configuration {
    pub channels: u32,
    pub packet_ms: u32,
    pub bitrate: u32,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            channels: 2,
            packet_ms: 10,
            bitrate: 128_000,
        }
    }
}

impl Configuration {
    pub fn validate(self) -> EncodeResult<Self> {
        if self.layout().is_none() {
            return Err("unsupported_channels");
        }
        if !matches!(self.packet_ms, 5 | 10 | 20) {
            return Err("unsupported_packet_duration");
        }
        let (minimum, maximum) = self.bitrate_range();
        if !(minimum..=maximum).contains(&self.bitrate) {
            return Err("unsupported_bitrate");
        }
        Ok(self)
    }

    pub fn packet_frames(self) -> usize {
        (SAMPLE_RATE / 1000 * self.packet_ms) as usize
    }

    pub fn layout(self) -> Option<ChannelLayout> {
        match self.channels {
            1 => Some(ChannelLayout {
                streams: 1,
                coupled_streams: 0,
                mapping: &[0],
                wave_mapping: &[0],
                vorbis_from_wave: &[0],
            }),
            2 => Some(ChannelLayout {
                streams: 1,
                coupled_streams: 1,
                mapping: &[0, 1],
                wave_mapping: &[0, 1],
                vorbis_from_wave: &[0, 1],
            }),
            // Opus family 1 uses Vorbis order. Public PCM retains WAVE order:
            // FL, FR, FC, LFE, BL, BR, then SL, SR.
            6 => Some(ChannelLayout {
                streams: 4,
                coupled_streams: 2,
                mapping: &[0, 4, 1, 2, 3, 5],
                wave_mapping: &[0, 1, 4, 5, 2, 3],
                vorbis_from_wave: &[0, 2, 1, 4, 5, 3],
            }),
            8 => Some(ChannelLayout {
                streams: 5,
                coupled_streams: 3,
                mapping: &[0, 6, 1, 2, 3, 4, 5, 7],
                wave_mapping: &[0, 1, 6, 7, 4, 5, 2, 3],
                vorbis_from_wave: &[0, 2, 1, 6, 7, 4, 5, 3],
            }),
            _ => None,
        }
    }

    pub fn bitrate_range(self) -> (u32, u32) {
        match self.channels {
            6 => (256_000, 1_536_000),
            8 if self.packet_ms == 5 => (512_000, 2_048_000),
            8 => (450_000, 2_048_000),
            _ => (6_000, 512_000),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Enumerate,
    Open {
        #[serde(default = "default_channels")]
        channels: u32,
        #[serde(default = "default_packet_ms")]
        packet_ms: u32,
        #[serde(default = "default_bitrate")]
        bitrate: u32,
        #[serde(default)]
        operation_id: Option<String>,
    },
    Lookup {
        operation_id: String,
    },
    Update {
        stream: u64,
        bitrate: u32,
    },
    Recover {
        stream: u64,
        #[serde(default)]
        bitrate: Option<u32>,
    },
    Close {
        stream: u64,
    },
    Status {
        stream: Option<u64>,
    },
}

fn default_channels() -> u32 {
    Configuration::default().channels
}
fn default_packet_ms() -> u32 {
    Configuration::default().packet_ms
}
fn default_bitrate() -> u32 {
    Configuration::default().bitrate
}

pub fn validate_operation(operation: &str) -> EncodeResult<()> {
    if operation.is_empty() || operation.len() > 128 || operation.chars().any(char::is_control) {
        return Err("invalid_operation_id");
    }
    Ok(())
}
