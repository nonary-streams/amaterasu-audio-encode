//! The only libopus ownership boundary. Encoder state is used exclusively through
//! mutable references; no C pointer or borrowed samples escape this wrapper.
use crate::model::{Configuration, EncodeResult, SAMPLE_RATE};
use opusic_sys as opus;
use std::ptr::NonNull;

pub const MAX_OPUS_PACKET: usize = 4000;

pub struct Encoder {
    pointer: NonNull<opus::OpusEncoder>,
    channels: usize,
    pub lookahead: u32,
    pub packet_bytes: usize,
}

// SAFETY: libopus encoder instances have no thread affinity. Ownership can move
// between threads, but all calls require exclusive access (the provider mutex).
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(configuration: Configuration) -> EncodeResult<Self> {
        let mut error = 0;
        // SAFETY: valid documented rate/channel/application values and live error output.
        let pointer = unsafe {
            opus::opus_encoder_create(
                SAMPLE_RATE as i32,
                configuration.channels as i32,
                opus::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut error,
            )
        };
        let pointer = NonNull::new(pointer).ok_or("codec_initialization_failed")?;
        let mut encoder = Self {
            pointer,
            channels: configuration.channels as usize,
            lookahead: 0,
            packet_bytes: 0,
        };
        if error != opus::OPUS_OK {
            return Err("codec_initialization_failed");
        }
        encoder.configure_constant_bitrate(configuration.bitrate)?;
        encoder.lookahead = encoder.read_lookahead()?;
        encoder.measure_packet_size(configuration.packet_frames())?;
        Ok(encoder)
    }

    fn configure_constant_bitrate(&mut self, bitrate: u32) -> EncodeResult<()> {
        // SAFETY: both requests take exactly one C int; the allocation is live.
        let bitrate_result = unsafe {
            opus::opus_encoder_ctl(
                self.pointer.as_ptr(),
                opus::OPUS_SET_BITRATE_REQUEST,
                bitrate as i32,
            )
        };
        // SAFETY: SET_VBR takes one C int. Fixed-size packets are required by
        // GameStream's audio recovery format; libopus performs its own padding.
        let cbr_result = unsafe {
            opus::opus_encoder_ctl(self.pointer.as_ptr(), opus::OPUS_SET_VBR_REQUEST, 0i32)
        };
        if bitrate_result != opus::OPUS_OK || cbr_result != opus::OPUS_OK {
            return Err("codec_configuration_failed");
        }
        Ok(())
    }

    fn read_lookahead(&self) -> EncodeResult<u32> {
        let mut lookahead: i32 = 0;
        // SAFETY: GET_LOOKAHEAD takes an initialized C int output pointer.
        let result = unsafe {
            opus::opus_encoder_ctl(
                self.pointer.as_ptr(),
                opus::OPUS_GET_LOOKAHEAD_REQUEST,
                &mut lookahead,
            )
        };
        if result != opus::OPUS_OK || !(0..=960).contains(&lookahead) {
            return Err("codec_configuration_failed");
        }
        Ok(lookahead as u32)
    }

    fn measure_packet_size(&mut self, frames: usize) -> EncodeResult<()> {
        // Ask the codec for this profile's packet size, including bitrate rounding.
        // Reset afterwards so no synthetic samples enter the caller's output.
        let silence = vec![0.0; frames * self.channels];
        self.packet_bytes = self.encode(&silence, frames)?.len();
        self.reset()
    }

    pub fn reset(&mut self) -> EncodeResult<()> {
        // SAFETY: RESET_STATE takes no variadic argument and the state is exclusively owned.
        let result =
            unsafe { opus::opus_encoder_ctl(self.pointer.as_ptr(), opus::OPUS_RESET_STATE) };
        if result == opus::OPUS_OK {
            Ok(())
        } else {
            Err("codec_reset_failed")
        }
    }

    pub fn encode(&mut self, samples: &[f32], frames: usize) -> EncodeResult<Vec<u8>> {
        if samples.len() != frames * self.channels || !matches!(frames, 240 | 480 | 960) {
            return Err("invalid_pcm");
        }
        let mut output = vec![0u8; MAX_OPUS_PACKET];
        // SAFETY: input contains exactly frames*channels initialized f32 samples;
        // output is writable to its declared capacity and neither is retained by C.
        let result = unsafe {
            opus::opus_encode_float(
                self.pointer.as_ptr(),
                samples.as_ptr(),
                frames as i32,
                output.as_mut_ptr(),
                output.len() as i32,
            )
        };
        if result <= 0 || result as usize > output.len() {
            return Err("codec_encode_failed");
        }
        if self.packet_bytes != 0 && result as usize != self.packet_bytes {
            return Err("codec_packet_size_changed");
        }
        output.truncate(result as usize);
        Ok(output)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: this uniquely owned allocation came from opus_encoder_create.
        unsafe { opus::opus_encoder_destroy(self.pointer.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_bitrate_packets_keep_their_size_across_signal_changes_and_resets() {
        for channels in [1, 2] {
            for packet_ms in [5, 10, 20] {
                for bitrate in [6000, 6100, 128_000, 256_000, 512_000] {
                    let configuration = Configuration {
                        channels,
                        packet_ms,
                        bitrate,
                    };
                    let mut encoder = Encoder::new(configuration).unwrap();
                    let frames = configuration.packet_frames();
                    let bytes = encoder.packet_bytes;
                    assert!(bytes > 0 && bytes <= MAX_OPUS_PACKET);
                    for phase in 0..8 {
                        let samples: Vec<f32> = (0..frames * channels as usize)
                            .map(|index| {
                                if phase % 2 == 0 {
                                    0.0
                                } else {
                                    (index as f32 * 0.071).sin() * 0.8
                                }
                            })
                            .collect();
                        let packet = encoder.encode(&samples, frames).unwrap();
                        assert_eq!(packet.len(), bytes);
                        // SAFETY: the codec-generated packet allocation is live
                        // and the decoder helper only reads its declared bytes.
                        assert_eq!(
                            unsafe {
                                opus::opus_packet_get_nb_samples(
                                    packet.as_ptr(),
                                    packet.len() as i32,
                                    SAMPLE_RATE as i32,
                                )
                            },
                            frames as i32
                        );
                        if phase == 3 {
                            encoder.reset().unwrap();
                        }
                    }
                }
            }
        }
    }
}
