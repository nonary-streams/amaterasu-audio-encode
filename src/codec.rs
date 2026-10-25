//! The only libopus ownership boundary. Encoder state is used exclusively through
//! mutable references; no C pointer or borrowed samples escape this wrapper.
use crate::model::{Configuration, EncodeResult, SAMPLE_RATE};
use opusic_sys as opus;
use std::ptr::NonNull;

pub const MAX_OPUS_PACKET: usize = 16_000;

enum CodecState {
    Single(NonNull<opus::OpusEncoder>),
    Multistream(NonNull<opus::OpusMSEncoder>),
}

pub struct Encoder {
    state: CodecState,
    channels: usize,
    vorbis_from_wave: &'static [usize],
    pub lookahead: u32,
    pub packet_bytes: usize,
}

// SAFETY: libopus encoder instances have no thread affinity. Ownership can move
// between threads, but all calls require exclusive access (the provider mutex).
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(configuration: Configuration) -> EncodeResult<Self> {
        let layout = configuration.layout().ok_or("unsupported_channels")?;
        let mut error = 0;
        let mut streams = 0;
        let mut coupled_streams = 0;
        let mut mapping = [0u8; 8];
        // SAFETY: validated channels and live output allocations satisfy the
        // native constructor. The surround API enables LFE-specific coding
        // and returns the RFC 7845 family-1 Vorbis mapping.
        let state = unsafe {
            if configuration.channels <= 2 {
                NonNull::new(opus::opus_encoder_create(
                    SAMPLE_RATE as i32,
                    configuration.channels as i32,
                    opus::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                    &mut error,
                ))
                .map(CodecState::Single)
            } else {
                NonNull::new(opus::opus_multistream_surround_encoder_create(
                    SAMPLE_RATE as i32,
                    configuration.channels as i32,
                    1,
                    &mut streams,
                    &mut coupled_streams,
                    mapping.as_mut_ptr(),
                    opus::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                    &mut error,
                ))
                .map(CodecState::Multistream)
            }
        };
        let state = state.ok_or("codec_initialization_failed")?;
        let mut encoder = Self {
            state,
            channels: configuration.channels as usize,
            vorbis_from_wave: layout.vorbis_from_wave,
            lookahead: 0,
            packet_bytes: 0,
        };
        if error != opus::OPUS_OK {
            return Err("codec_initialization_failed");
        }
        if configuration.channels > 2
            && (streams != layout.streams
                || coupled_streams != layout.coupled_streams
                || mapping[..configuration.channels as usize] != *layout.mapping)
        {
            return Err("codec_mapping_mismatch");
        }
        encoder.configure_constant_bitrate(configuration.bitrate)?;
        encoder.lookahead = encoder.read_lookahead()?;
        encoder.measure_packet_size(configuration.packet_frames())?;
        Ok(encoder)
    }

    fn configure_constant_bitrate(&mut self, bitrate: u32) -> EncodeResult<()> {
        self.set_bitrate(bitrate)?;
        // SAFETY: SET_VBR takes one C int. Fixed-size packets are required by
        // GameStream's audio recovery format; libopus performs its own padding.
        let cbr_result = match self.state {
            CodecState::Single(pointer) => unsafe {
                opus::opus_encoder_ctl(pointer.as_ptr(), opus::OPUS_SET_VBR_REQUEST, 0i32)
            },
            CodecState::Multistream(pointer) => unsafe {
                opus::opus_multistream_encoder_ctl(
                    pointer.as_ptr(),
                    opus::OPUS_SET_VBR_REQUEST,
                    0i32,
                )
            },
        };
        if cbr_result != opus::OPUS_OK {
            return Err("codec_configuration_failed");
        }
        Ok(())
    }

    fn set_bitrate(&mut self, bitrate: u32) -> EncodeResult<()> {
        // SAFETY: SET_BITRATE takes exactly one C int and the owned state is live.
        let result = match self.state {
            CodecState::Single(pointer) => unsafe {
                opus::opus_encoder_ctl(
                    pointer.as_ptr(),
                    opus::OPUS_SET_BITRATE_REQUEST,
                    bitrate as i32,
                )
            },
            CodecState::Multistream(pointer) => unsafe {
                opus::opus_multistream_encoder_ctl(
                    pointer.as_ptr(),
                    opus::OPUS_SET_BITRATE_REQUEST,
                    bitrate as i32,
                )
            },
        };
        if result != opus::OPUS_OK {
            return Err("codec_configuration_failed");
        }
        Ok(())
    }

    pub fn update_bitrate(
        &mut self,
        previous_bitrate: u32,
        bitrate: u32,
        packet_bytes: usize,
    ) -> EncodeResult<()> {
        self.update_bitrate_with(previous_bitrate, bitrate, packet_bytes, Self::set_bitrate)
    }

    fn update_bitrate_with(
        &mut self,
        previous_bitrate: u32,
        bitrate: u32,
        packet_bytes: usize,
        mut set_bitrate: impl FnMut(&mut Self, u32) -> EncodeResult<()>,
    ) -> EncodeResult<()> {
        if set_bitrate(self, bitrate).is_err() {
            return if set_bitrate(self, previous_bitrate).is_ok() {
                Err("codec_configuration_failed")
            } else {
                Err("codec_bitrate_rollback_failed")
            };
        }
        self.packet_bytes = packet_bytes;
        Ok(())
    }

    fn read_lookahead(&self) -> EncodeResult<u32> {
        let mut lookahead: i32 = 0;
        // SAFETY: GET_LOOKAHEAD takes an initialized C int output pointer.
        let result = match self.state {
            CodecState::Single(pointer) => unsafe {
                opus::opus_encoder_ctl(
                    pointer.as_ptr(),
                    opus::OPUS_GET_LOOKAHEAD_REQUEST,
                    &mut lookahead,
                )
            },
            CodecState::Multistream(pointer) => unsafe {
                opus::opus_multistream_encoder_ctl(
                    pointer.as_ptr(),
                    opus::OPUS_GET_LOOKAHEAD_REQUEST,
                    &mut lookahead,
                )
            },
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
        let result = match self.state {
            CodecState::Single(pointer) => unsafe {
                opus::opus_encoder_ctl(pointer.as_ptr(), opus::OPUS_RESET_STATE)
            },
            CodecState::Multistream(pointer) => unsafe {
                opus::opus_multistream_encoder_ctl(pointer.as_ptr(), opus::OPUS_RESET_STATE)
            },
        };
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
        let result = match self.state {
            CodecState::Single(pointer) => unsafe {
                opus::opus_encode_float(
                    pointer.as_ptr(),
                    samples.as_ptr(),
                    frames as i32,
                    output.as_mut_ptr(),
                    output.len() as i32,
                )
            },
            CodecState::Multistream(pointer) => unsafe {
                let reordered = samples
                    .chunks_exact(self.channels)
                    .flat_map(|frame| self.vorbis_from_wave.iter().map(|index| frame[*index]))
                    .collect::<Vec<_>>();
                opus::opus_multistream_encode_float(
                    pointer.as_ptr(),
                    reordered.as_ptr(),
                    frames as i32,
                    output.as_mut_ptr(),
                    output.len() as i32,
                )
            },
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
        // SAFETY: each uniquely owned allocation is destroyed by its matching
        // libopus API, including partially configured encoder instances.
        match self.state {
            CodecState::Single(pointer) => unsafe { opus::opus_encoder_destroy(pointer.as_ptr()) },
            CodecState::Multistream(pointer) => unsafe {
                opus::opus_multistream_encoder_destroy(pointer.as_ptr())
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_bitrate_change_rolls_back_without_changing_packet_metadata() {
        let profile = Configuration::default();
        let mut encoder = Encoder::new(profile).unwrap();
        let original_size = encoder.packet_bytes;
        let mut requests = Vec::new();
        let result = encoder.update_bitrate_with(
            profile.bitrate,
            64_000,
            original_size / 2,
            |encoder, bitrate| {
                requests.push(bitrate);
                if bitrate == 64_000 {
                    Err("codec_configuration_failed")
                } else {
                    encoder.set_bitrate(bitrate)
                }
            },
        );
        assert_eq!(result, Err("codec_configuration_failed"));
        assert_eq!(requests, [64_000, profile.bitrate]);
        assert_eq!(encoder.packet_bytes, original_size);
        let samples = vec![0.0; profile.packet_frames() * profile.channels as usize];
        assert_eq!(
            encoder
                .encode(&samples, profile.packet_frames())
                .unwrap()
                .len(),
            original_size
        );
    }

    #[test]
    fn failed_bitrate_rollback_is_reported_without_committing_packet_metadata() {
        let profile = Configuration::default();
        let mut encoder = Encoder::new(profile).unwrap();
        let original_size = encoder.packet_bytes;
        let mut requests = Vec::new();
        let result = encoder.update_bitrate_with(
            profile.bitrate,
            64_000,
            original_size / 2,
            |_, bitrate| {
                requests.push(bitrate);
                Err("codec_configuration_failed")
            },
        );
        assert_eq!(result, Err("codec_bitrate_rollback_failed"));
        assert_eq!(requests, [64_000, profile.bitrate]);
        assert_eq!(encoder.packet_bytes, original_size);
    }

    #[test]
    fn live_bitrate_updates_match_premeasured_cbr_sizes_without_reset() {
        for channels in [1, 2, 6, 8] {
            for packet_ms in [5, 10, 20] {
                let profile = Configuration {
                    channels,
                    packet_ms,
                    bitrate: if channels > 2 { 1_024_000 } else { 128_000 },
                };
                let mut encoder = Encoder::new(profile).unwrap();
                let mut current_bitrate = profile.bitrate;
                let frames = profile.packet_frames();
                let samples = vec![0.25; frames * channels as usize];
                assert_eq!(
                    encoder.encode(&samples, frames).unwrap().len(),
                    encoder.packet_bytes
                );
                for bitrate in if channels > 2 {
                    [768_000, 1_536_000]
                } else {
                    [64_000, 256_000]
                } {
                    let expected = Encoder::new(Configuration { bitrate, ..profile }).unwrap();
                    encoder
                        .update_bitrate(current_bitrate, bitrate, expected.packet_bytes)
                        .unwrap();
                    current_bitrate = bitrate;
                    assert_eq!(
                        encoder.encode(&samples, frames).unwrap().len(),
                        expected.packet_bytes
                    );
                }
            }
        }
    }

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

    #[test]
    fn multistream_profiles_decode_every_channel_at_each_packet_duration() {
        for (channels, bitrates) in [
            (6, [256_000, 512_000, 1_536_000]),
            (8, [450_000, 512_000, 2_048_000]),
        ] {
            for packet_ms in [5, 10, 20] {
                for bitrate in bitrates {
                    if channels == 8 && packet_ms == 5 && bitrate == 450_000 {
                        continue;
                    }
                    let configuration = Configuration {
                        channels,
                        packet_ms,
                        bitrate,
                    };
                    let layout = configuration.layout().unwrap();
                    let (expected_mapping, vorbis_to_wave): (&[u8], &[usize]) = if channels == 6 {
                        (&[0, 4, 1, 2, 3, 5], &[0, 2, 1, 4, 5, 3])
                    } else {
                        (&[0, 6, 1, 2, 3, 4, 5, 7], &[0, 2, 1, 6, 7, 4, 5, 3])
                    };
                    assert_eq!(layout.mapping, expected_mapping);
                    let mut encoder = Encoder::new(configuration).unwrap();
                    let mut error = 0;
                    // SAFETY: the static mapping outlives this independently
                    // allocated decoder; all channel and stream counts are valid.
                    let decoder = unsafe {
                        opus::opus_multistream_decoder_create(
                            SAMPLE_RATE as i32,
                            channels as i32,
                            layout.streams,
                            layout.coupled_streams,
                            expected_mapping.as_ptr(),
                            &mut error,
                        )
                    };
                    assert_eq!(error, opus::OPUS_OK);
                    assert!(!decoder.is_null());
                    let frames = configuration.packet_frames();
                    let mut channel_energy = vec![0.0f64; channels as usize];
                    let mut decoded_pcm = Vec::new();
                    for packet_index in 0..12 {
                        let samples = (0..frames)
                            .flat_map(|sample| {
                                (0..channels).map(move |channel| {
                                    let frequency = if channel == 3 {
                                        60.0
                                    } else {
                                        170.0 + 83.0 * channel as f32
                                    };
                                    let phase = (packet_index * frames + sample) as f32
                                        * frequency
                                        * std::f32::consts::TAU
                                        / SAMPLE_RATE as f32;
                                    phase.sin() * 0.5
                                })
                            })
                            .collect::<Vec<_>>();
                        let bytes = encoder.encode(&samples, frames).unwrap();
                        assert_eq!(bytes.len(), encoder.packet_bytes);
                        let mut output = vec![0.0f32; frames * channels as usize];
                        // SAFETY: the packet and output buffer remain live for
                        // this synchronous decode; frame capacity is exact.
                        let decoded = unsafe {
                            opus::opus_multistream_decode_float(
                                decoder,
                                bytes.as_ptr(),
                                bytes.len() as i32,
                                output.as_mut_ptr(),
                                frames as i32,
                                0,
                            )
                        };
                        assert_eq!(decoded, frames as i32);
                        for sample in output.chunks_exact(channels as usize) {
                            for (energy, value) in channel_energy.iter_mut().zip(sample) {
                                *energy += f64::from(*value).powi(2);
                            }
                        }
                        decoded_pcm.extend(output);
                    }
                    assert!(
                        channel_energy.iter().all(|energy| *energy > 1.0),
                        "channels={channels} packet_ms={packet_ms} bitrate={bitrate} energy={channel_energy:?}"
                    );
                    // The independent decoder must retain the channel order,
                    // rather than merely producing some nonzero output.
                    for (channel, &wave_channel) in vorbis_to_wave.iter().enumerate() {
                        if wave_channel == 3 {
                            // LFE is encoded with a deliberately narrow band
                            // and a small rate; its isolation is tested below.
                            continue;
                        }
                        let amplitudes = (0..channels as usize)
                            .map(|tone_channel| {
                                let frequency = if tone_channel == 3 {
                                    60.0
                                } else {
                                    170.0 + 83.0 * tone_channel as f64
                                };
                                let (sin, cos) = decoded_pcm
                                    .chunks_exact(channels as usize)
                                    .skip(2 * frames)
                                    .enumerate()
                                    .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                                        let phase =
                                            index as f64 * frequency * std::f64::consts::TAU
                                                / f64::from(SAMPLE_RATE);
                                        let value = f64::from(sample[channel]);
                                        (sin + value * phase.sin(), cos + value * phase.cos())
                                    });
                                sin.hypot(cos)
                            })
                            .collect::<Vec<_>>();
                        let other = amplitudes
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| *index != wave_channel)
                            .map(|(_, amplitude)| *amplitude)
                            .fold(0.0f64, f64::max);
                        assert!(
                            amplitudes[wave_channel] > other * 1.5,
                            "channels={channels} packet_ms={packet_ms} bitrate={bitrate} vorbis_channel={channel} wave_channel={wave_channel} amplitudes={amplitudes:?}"
                        );
                    }
                    // SAFETY: the decoder is uniquely owned by this test.
                    unsafe { opus::opus_multistream_decoder_destroy(decoder) };
                }
            }
        }
    }

    #[test]
    fn surround_lfe_and_center_remain_separate_speakers() {
        for (channels, bitrate, mapping) in [
            (6, 256_000, &[0u8, 4, 1, 2, 3, 5][..]),
            (8, 450_000, &[0u8, 6, 1, 2, 3, 4, 5, 7][..]),
        ] {
            let layout = Configuration {
                channels,
                packet_ms: 10,
                bitrate,
            }
            .layout()
            .unwrap();
            assert_eq!(layout.mapping, mapping);
            // Decoder channel indices 0/1 are coupled front L/R. In 5.1,
            // 2/3 are rear L/R; in 7.1, 2/3 are side L/R and 4/5 rear L/R.
            assert_eq!(&layout.wave_mapping[..2], &[0, 1]);
            if channels == 6 {
                assert_eq!(&layout.wave_mapping[4..6], &[2, 3]);
            } else {
                assert_eq!(&layout.wave_mapping[6..8], &[2, 3]);
                assert_eq!(&layout.wave_mapping[4..6], &[4, 5]);
            }
            assert_eq!(layout.wave_mapping[2], (2 * layout.coupled_streams) as u8);
            assert_eq!(
                layout.wave_mapping[3],
                (layout.streams + layout.coupled_streams - 1) as u8
            );
            for (wave_speaker, frequency, vorbis_speaker) in
                [(3, 60.0f32, channels as usize - 1), (2, 400.0, 1)]
            {
                let configuration = Configuration {
                    channels,
                    packet_ms: 10,
                    bitrate,
                };
                let mut encoder = Encoder::new(configuration).unwrap();
                let mut error = 0;
                // SAFETY: this independent decoder uses the RFC family-1
                // mapping, not the encoder's reported map, and remains live.
                let decoder = unsafe {
                    opus::opus_multistream_decoder_create(
                        SAMPLE_RATE as i32,
                        channels as i32,
                        configuration.layout().unwrap().streams,
                        configuration.layout().unwrap().coupled_streams,
                        mapping.as_ptr(),
                        &mut error,
                    )
                };
                assert_eq!(error, opus::OPUS_OK);
                assert!(!decoder.is_null());
                let wave_decoder = unsafe {
                    opus::opus_multistream_decoder_create(
                        SAMPLE_RATE as i32,
                        channels as i32,
                        layout.streams,
                        layout.coupled_streams,
                        layout.wave_mapping.as_ptr(),
                        &mut error,
                    )
                };
                assert_eq!(error, opus::OPUS_OK);
                assert!(!wave_decoder.is_null());
                let frames = configuration.packet_frames();
                let mut energy = vec![0.0f64; channels as usize];
                let mut wave_energy = vec![0.0f64; channels as usize];
                for packet_index in 0..20 {
                    let mut input = vec![0.0f32; frames * channels as usize];
                    for frame in 0..frames {
                        let phase = (packet_index * frames + frame) as f32
                            * frequency
                            * std::f32::consts::TAU
                            / SAMPLE_RATE as f32;
                        input[frame * channels as usize + wave_speaker] = phase.sin() * 0.5;
                    }
                    let bytes = encoder.encode(&input, frames).unwrap();
                    let mut output = vec![0.0f32; input.len()];
                    let mut wave_output = vec![0.0f32; input.len()];
                    // SAFETY: live packet and full frame output allocation.
                    assert_eq!(
                        unsafe {
                            opus::opus_multistream_decode_float(
                                decoder,
                                bytes.as_ptr(),
                                bytes.len() as i32,
                                output.as_mut_ptr(),
                                frames as i32,
                                0,
                            )
                        },
                        frames as i32
                    );
                    // SAFETY: separate live decoder and full frame output.
                    assert_eq!(
                        unsafe {
                            opus::opus_multistream_decode_float(
                                wave_decoder,
                                bytes.as_ptr(),
                                bytes.len() as i32,
                                wave_output.as_mut_ptr(),
                                frames as i32,
                                0,
                            )
                        },
                        frames as i32
                    );
                    for sample in output.chunks_exact(channels as usize) {
                        for (accumulator, value) in energy.iter_mut().zip(sample) {
                            *accumulator += f64::from(*value).powi(2);
                        }
                    }
                    for sample in wave_output.chunks_exact(channels as usize) {
                        for (accumulator, value) in wave_energy.iter_mut().zip(sample) {
                            *accumulator += f64::from(*value).powi(2);
                        }
                    }
                }
                assert!(energy[vorbis_speaker] > 1.0, "energy={energy:?}");
                assert!(
                    wave_energy[wave_speaker] > 1.0,
                    "wave_energy={wave_energy:?}"
                );
                for (speaker, other) in energy.iter().enumerate() {
                    if speaker != vorbis_speaker {
                        assert!(
                            *other < energy[vorbis_speaker] * 0.01,
                            "channels={channels} speaker={speaker} energy={energy:?}"
                        );
                    }
                }
                for (speaker, other) in wave_energy.iter().enumerate() {
                    if speaker != wave_speaker {
                        assert!(
                            *other < wave_energy[wave_speaker] * 0.01,
                            "channels={channels} speaker={speaker} wave_energy={wave_energy:?}"
                        );
                    }
                }
                // SAFETY: unique independent decoder allocation.
                unsafe { opus::opus_multistream_decoder_destroy(decoder) };
                // SAFETY: separate unique decoder allocation.
                unsafe { opus::opus_multistream_decoder_destroy(wave_decoder) };
            }
        }
    }
}
