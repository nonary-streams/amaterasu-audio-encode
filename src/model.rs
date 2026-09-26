use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;
pub const MAX_PACKETS: usize = 24;
pub type EncodeResult<T> = Result<T, &'static str>;

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
        if !matches!(self.channels, 1 | 2) {
            return Err("unsupported_channels");
        }
        if !matches!(self.packet_ms, 5 | 10 | 20) {
            return Err("unsupported_packet_duration");
        }
        if !(6000..=512_000).contains(&self.bitrate) {
            return Err("unsupported_bitrate");
        }
        Ok(self)
    }

    pub fn packet_frames(self) -> usize {
        (SAMPLE_RATE / 1000 * self.packet_ms) as usize
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
