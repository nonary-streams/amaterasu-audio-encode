use super::*;
use opusic_sys as opus;
use std::ffi::c_void;

#[derive(Clone, Debug, PartialEq)]
struct Delivered {
    bytes: Vec<u8>,
    flags: u32,
    frames: u32,
    timestamp: u64,
    sequence: u64,
}

struct Sink {
    packets: Vec<Delivered>,
    reject: bool,
}

impl Sink {
    fn new() -> Self {
        Self {
            packets: Vec::new(),
            reject: false,
        }
    }
    fn native(&mut self) -> AudioPacketSink {
        unsafe extern "C" fn write(
            context: *mut c_void,
            packet: *const EncodedAudioPacket,
        ) -> Status {
            // SAFETY: tests keep the sink and packet alive throughout delivery.
            let sink = unsafe { &mut *context.cast::<Sink>() };
            let packet = unsafe { &*packet };
            sink.packets.push(Delivered {
                bytes: unsafe { packet.bytes().unwrap() }.to_vec(),
                flags: packet.flags,
                frames: packet.frames,
                timestamp: packet.timestamp_us,
                sequence: packet.sequence,
            });
            if sink.reject { MEDIA_BUSY } else { STATUS_OK }
        }
        AudioPacketSink {
            context: (self as *mut Self).cast(),
            write,
        }
    }
}

fn input(bytes: &[u8], channels: u32, format: u32, timestamp_us: u64, sequence: u64) -> AudioFrame {
    AudioFrame {
        sample_rate: SAMPLE_RATE,
        channels,
        format,
        frames: (bytes.len() / channels as usize / if format == PCM_F32 { 4 } else { 2 }) as u32,
        flags: 0,
        data: bytes.as_ptr(),
        len: bytes.len(),
        timestamp_us,
        sequence,
    }
}

fn stream(channels: u32, packet_ms: u32) -> Stream {
    Stream::new(
        "test".into(),
        Configuration {
            channels,
            packet_ms,
            bitrate: 128_000,
        },
    )
    .unwrap()
}

struct Decoder(*mut opus::OpusDecoder);
impl Decoder {
    fn new(channels: u32) -> Self {
        let mut error = 0;
        // SAFETY: documented rate/channels, live error output; independently
        // allocated reference decoder shares no state with the encoder wrapper.
        let pointer = unsafe { opus::opus_decoder_create(48_000, channels as i32, &mut error) };
        assert_eq!(error, opus::OPUS_OK);
        assert!(!pointer.is_null());
        Self(pointer)
    }
    fn decode(&mut self, packet: &Delivered, channels: u32) -> Vec<f32> {
        // SAFETY: live packet bytes and independently owned decoder, with output
        // allocation large enough for the maximum permitted packet duration.
        let count = unsafe {
            opus::opus_packet_get_nb_samples(
                packet.bytes.as_ptr(),
                packet.bytes.len() as i32,
                48_000,
            )
        };
        assert_eq!(count, packet.frames as i32);
        let mut output = vec![0.0; 5760 * channels as usize];
        let decoded = unsafe {
            opus::opus_decode_float(
                self.0,
                packet.bytes.as_ptr(),
                packet.bytes.len() as i32,
                output.as_mut_ptr(),
                5760,
                0,
            )
        };
        assert_eq!(decoded, count);
        output.truncate(decoded as usize * channels as usize);
        output
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: unique decoder returned by opus_decoder_create.
        unsafe { opus::opus_decoder_destroy(self.0) };
    }
}

#[test]
fn genuine_opus_roundtrip_preserves_mono_stereo_tones_and_timing_for_all_durations() {
    for channels in [1, 2] {
        for packet_ms in [5, 10, 20] {
            for format in [PCM_F32, PCM_I16] {
                let mut encoder = stream(channels, packet_ms);
                let mut sink = Sink::new();
                let mut original = Vec::new();
                let mut decoder = Decoder::new(channels);
                let mut decoded = Vec::new();
                for chunk in 0..20u64 {
                    let samples: Vec<f32> = (0..240)
                        .flat_map(|i| {
                            (0..channels).map(move |channel| {
                                let phase = (chunk * 240 + i) as f32
                                    * (440.0 + 220.0 * channel as f32)
                                    * std::f32::consts::TAU
                                    / 48_000.0;
                                phase.sin() * 0.5
                            })
                        })
                        .collect();
                    original.extend_from_slice(&samples);
                    let bytes: Vec<u8> = if format == PCM_F32 {
                        samples
                            .iter()
                            .flat_map(|sample| sample.to_le_bytes())
                            .collect()
                    } else {
                        samples
                            .iter()
                            .flat_map(|sample| ((*sample * 32767.0) as i16).to_le_bytes())
                            .collect()
                    };
                    let frame = input(&bytes, channels, format, 1000 + chunk * 5000, chunk);
                    // SAFETY: local bytes and sink allocations outlive each callback.
                    assert_eq!(unsafe { encoder.submit(&frame) }, STATUS_OK);
                    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
                }
                assert_eq!(encoder.drain(), STATUS_OK);
                while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
                for (index, packet) in sink
                    .packets
                    .iter()
                    .filter(|packet| packet.frames != 0)
                    .enumerate()
                {
                    assert_eq!(packet.sequence, index as u64);
                    assert_eq!(
                        packet.timestamp,
                        1000 + index as u64 * packet_ms as u64 * 1000
                    );
                    decoded.extend(decoder.decode(packet, channels));
                }
                let delay = encoder.lookahead() as usize * channels as usize;
                assert!(decoded.len() >= original.len() + delay);
                let signal: f64 = original.iter().map(|x| f64::from(*x).powi(2)).sum();
                let error: f64 = original
                    .iter()
                    .zip(&decoded[delay..])
                    .map(|(a, b)| f64::from(a - b).powi(2))
                    .sum();
                assert!(
                    10.0 * (signal / error).log10() > 15.0,
                    "poor roundtrip channels={channels} duration={packet_ms}"
                );
                assert_eq!(
                    sink.packets.last().unwrap().flags,
                    AUDIO_PACKET_END_OF_STREAM
                );
            }
        }
    }
}

#[test]
fn partial_i16_padding_rejected_sink_and_eos_are_retry_safe() {
    let mut encoder = stream(2, 10);
    let bytes = vec![0u8; 240 * 2 * 2];
    let first = input(&bytes, 2, PCM_I16, 0, 1);
    let mut sink = Sink::new();
    // SAFETY: test allocations remain live throughout each synchronous call.
    assert_eq!(unsafe { encoder.submit(&first) }, STATUS_OK);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_PENDING);
    assert_eq!(
        unsafe {
            encoder.submit(&AudioFrame {
                timestamp_us: 5000,
                sequence: 2,
                ..first
            })
        },
        STATUS_OK
    );
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    sink.reject = false;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0], sink.packets[1]);
    let tail = vec![0u8; 120 * 2 * 2];
    assert_eq!(
        unsafe { encoder.submit(&input(&tail, 2, PCM_I16, 10_000, 3)) },
        STATUS_OK
    );
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(encoder.padded_frames, 360);
    assert_eq!(unsafe { encoder.submit(&first) }, MEDIA_CLOSED);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets.last().unwrap().frames, 480);
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    sink.reject = false;
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(
        sink.packets[sink.packets.len() - 1],
        sink.packets[sink.packets.len() - 2]
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn failed_stream_recovery_discards_uncertain_output_and_resumes_on_a_new_epoch() {
    let mut encoder = stream(1, 10);
    assert_eq!(encoder.recover(), Err("stream_not_failed"));
    let full = vec![0u8; 480 * 2];
    let partial = vec![0u8; 120 * 2];
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 1000, 0)) },
        STATUS_OK
    );
    assert_eq!(
        unsafe { encoder.submit(&input(&partial, 1, PCM_I16, 11_000, 1)) },
        STATUS_OK
    );
    assert_eq!(encoder.queued(), 1);
    encoder.failure = Some("codec_encode_failed");
    let mut sink = Sink::new();
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    assert_eq!(encoder.queued(), 1);

    assert_eq!(
        encoder.recover_with(|_| Err("codec_initialization_failed")),
        Err("codec_initialization_failed")
    );
    assert_eq!(encoder.state(), "failed");
    assert_eq!(encoder.queued(), 1);
    assert_eq!(encoder.buffered_frames(), 120);
    assert_eq!(encoder.last_input_timestamp_us, Some(11_000));

    encoder.recover().unwrap();
    assert_eq!(encoder.state(), "encoding");
    assert_eq!(encoder.queued(), 1);
    assert_eq!(encoder.buffered_frames(), 0);
    assert_eq!(encoder.discarded_frames, 120);
    assert_eq!(encoder.last_input_timestamp_us, None);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    sink.reject = false;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_PENDING);
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 200_000, 50)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets.len(), 4);
    assert_eq!(sink.packets[3].sequence, 1);
    assert_eq!(sink.packets[3].timestamp, 200_000);
    assert_eq!(sink.packets[3].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(encoder.drain(), STATUS_OK);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
}

#[test]
fn failed_encoder_can_reopen_at_a_new_bitrate_after_preserving_prior_packets() {
    let mut encoder = stream(1, 10);
    let original_size = encoder.packet_bytes();
    let bytes = vec![0u8; 480 * 2];
    let first = input(&bytes, 1, PCM_I16, 1_000, 0);
    // SAFETY: local PCM and sink allocations live through synchronous calls.
    assert_eq!(unsafe { encoder.submit(&first) }, STATUS_OK);
    encoder.failure = Some("codec_encode_failed");
    assert_eq!(
        encoder.recover_with_bitrate(Some(64_000)),
        Err("stream_busy")
    );
    assert_eq!(encoder.configuration.bitrate, 128_000);
    assert_eq!(
        encoder.recover_with_bitrate(Some(5_999)),
        Err("unsupported_bitrate")
    );
    assert_eq!(encoder.state(), "failed");
    let mut sink = Sink::new();
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0].bytes.len(), original_size);
    assert_eq!(
        unsafe { encoder.receive(sink.native()) },
        MEDIA_BACKEND_ERROR
    );
    assert_eq!(
        encoder.recover_with_configuration(Some(64_000), |_| Err("codec_initialization_failed")),
        Err("codec_initialization_failed")
    );
    assert_eq!(encoder.configuration.bitrate, 128_000);
    assert_eq!(encoder.state(), "failed");

    encoder.recover_with_bitrate(Some(64_000)).unwrap();
    assert_eq!(encoder.state(), "encoding");
    assert_eq!(encoder.configuration.bitrate, 64_000);
    assert_ne!(encoder.packet_bytes(), original_size);
    let next = input(&bytes, 1, PCM_I16, 100_000, 50);
    assert_eq!(unsafe { encoder.submit(&next) }, STATUS_OK);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[1].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(sink.packets[1].sequence, 1);
    assert_eq!(sink.packets[1].bytes.len(), encoder.packet_bytes());
    assert_eq!(encoder.update_bitrate(96_000), Ok(()));
    assert_eq!(encoder.drain(), STATUS_OK);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn failure_delivers_prior_packets_with_retry_before_reporting_error_and_recovery_resumes() {
    let mut encoder = stream(1, 10);
    let full = vec![0u8; 480 * 2];
    for sequence in 0..2 {
        assert_eq!(
            unsafe { encoder.submit(&input(&full, 1, PCM_I16, sequence * 10_000, sequence)) },
            STATUS_OK
        );
    }
    encoder.failure = Some("codec_encode_failed");
    let mut sink = Sink::new();
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    assert_eq!(encoder.queued(), 2);
    sink.reject = false;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0], sink.packets[1]);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[2].sequence, 1);
    assert_eq!(
        unsafe { encoder.receive(sink.native()) },
        MEDIA_BACKEND_ERROR
    );
    assert_eq!(encoder.drain(), MEDIA_BACKEND_ERROR);
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 20_000, 2)) },
        MEDIA_BACKEND_ERROR
    );
    assert!(
        sink.packets
            .iter()
            .all(|packet| packet.flags & AUDIO_PACKET_END_OF_STREAM == 0)
    );

    encoder.recover().unwrap();
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 100_000, 100)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[3].sequence, 2);
    assert_eq!(sink.packets[3].timestamp, 100_000);
    assert_eq!(sink.packets[3].flags, AUDIO_PACKET_DISCONTINUITY);
}

#[test]
fn draining_stream_cannot_be_recovered_as_a_failure() {
    let mut encoder = stream(1, 10);
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(encoder.recover(), Err("stream_not_failed"));
    assert_eq!(encoder.state(), "draining");
    let mut sink = Sink::new();
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
    assert_eq!(encoder.recover(), Err("stream_not_failed"));
    assert_eq!(encoder.state(), "ended");
}

#[test]
fn failure_never_delivers_a_queued_end_of_stream_marker() {
    let mut encoder = stream(1, 10);
    assert_eq!(encoder.drain(), STATUS_OK);
    encoder.failure = Some("codec_encode_failed");
    let mut sink = Sink::new();
    assert_eq!(
        unsafe { encoder.receive(sink.native()) },
        MEDIA_BACKEND_ERROR
    );
    assert!(sink.packets.is_empty());
    assert_eq!(encoder.queued(), 1);
}

#[test]
fn recovery_removes_failed_drain_end_marker_but_keeps_earlier_audio() {
    let mut encoder = stream(1, 10);
    let bytes = vec![0u8; 480 * 2];
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 0, 0)) },
        STATUS_OK
    );
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(encoder.queued(), 3);
    encoder.failure = Some("codec_encode_failed");
    encoder.recover().unwrap();
    assert_eq!(encoder.queued(), 2);
    let mut sink = Sink::new();
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0].sequence, 0);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[1].sequence, 1);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_PENDING);
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 100_000, 10)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[2].sequence, 2);
    assert_eq!(sink.packets[2].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(encoder.drain(), STATUS_OK);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
}

#[test]
fn immediate_drain_after_recovery_places_eos_after_retained_audio() {
    let mut encoder = stream(1, 10);
    let bytes = vec![0u8; 480 * 2];
    let timestamp = 1_000_000;
    // SAFETY: local PCM and sink allocations live through synchronous calls.
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, timestamp, 0)) },
        STATUS_OK
    );
    assert_eq!(encoder.queued(), 1);
    encoder.failure = Some("codec_encode_failed");
    encoder.recover().unwrap();
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(encoder.queued(), 2);

    let mut sink = Sink::new();
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    sink.reject = false;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0], sink.packets[1]);
    assert_eq!(sink.packets[1].timestamp, timestamp);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[2].flags, AUDIO_PACKET_END_OF_STREAM);
    assert_eq!(sink.packets[2].sequence, 1);
    assert_eq!(sink.packets[2].timestamp, timestamp + 10_000);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn drained_recovery_keeps_eos_clock_after_retained_audio_was_delivered() {
    let mut encoder = stream(1, 10);
    let bytes = vec![0u8; 480 * 2];
    let timestamp = 1_000_000;
    let frame = input(&bytes, 1, PCM_I16, timestamp, 0);
    let mut sink = Sink::new();

    // The first real Opus packet survives a failed flush. A rejected delivery
    // must remain available for retry before the caller recovers the stream.
    assert_eq!(unsafe { encoder.submit(&frame) }, STATUS_OK);
    assert_eq!(
        encoder.drain_with(|_, _, _| Err("codec_encode_failed")),
        MEDIA_BACKEND_ERROR
    );
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    sink.reject = false;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0], sink.packets[1]);
    assert_eq!(sink.packets[1].timestamp, timestamp);
    assert_eq!(
        unsafe { encoder.receive(sink.native()) },
        MEDIA_BACKEND_ERROR
    );

    encoder.recover().unwrap();
    assert_eq!(encoder.queued(), 0);
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(unsafe { encoder.submit(&frame) }, MEDIA_CLOSED);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[2].flags, AUDIO_PACKET_END_OF_STREAM);
    assert_eq!(sink.packets[2].sequence, 1);
    assert_eq!(sink.packets[2].timestamp, timestamp + 10_000);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn failed_drain_recovery_update_and_eos_preserve_input_accounting_and_decoder_reset() {
    let mut encoder = stream(1, 10);
    assert_eq!(encoder.update_bitrate(64_000), Ok(()));
    let full = vec![0u8; 480 * 2];
    let partial = vec![0u8; 120 * 2];
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 1_000, 0)) },
        STATUS_OK
    );
    assert_eq!(
        unsafe { encoder.submit(&input(&partial, 1, PCM_I16, 11_000, 1)) },
        STATUS_OK
    );
    // Force the codec's packet-size check to fail on flush. The first packet
    // remains valid while the added silence must not become discarded input.
    encoder.encoder.packet_bytes += 1;
    assert_eq!(encoder.drain(), MEDIA_BACKEND_ERROR);
    assert_eq!(encoder.state(), "failed");
    assert_eq!(encoder.buffered_frames(), 120);
    assert_eq!(encoder.padded_frames, 0);
    assert_eq!(encoder.queued(), 1);

    encoder.recover().unwrap();
    assert_eq!(encoder.discarded_frames, 120);
    assert_eq!(encoder.buffered_frames(), 0);
    assert_eq!(encoder.update_bitrate(96_000), Err("stream_busy"));
    let mut sink = Sink::new();
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0].timestamp, 1_000);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_PENDING);
    assert_eq!(encoder.update_bitrate(96_000), Ok(()));
    assert_eq!(encoder.configuration.bitrate, 96_000);
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 100_000, 20)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[1].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(sink.packets[1].sequence, 1);
    assert_eq!(encoder.drain(), STATUS_OK);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn failed_second_flush_keeps_queued_audio_and_counts_emitted_padding() {
    let mut encoder = stream(1, 10);
    let partial = vec![0u8; 400 * 2];
    let full = vec![0u8; 480 * 2];
    let mut sink = Sink::new();
    assert_eq!(
        unsafe { encoder.submit(&input(&partial, 1, PCM_I16, 1_000, 0)) },
        STATUS_OK
    );

    let mut attempts = 0;
    assert_eq!(
        encoder.drain_with(|codec, samples, frames| {
            attempts += 1;
            if attempts == 2 {
                Err("codec_encode_failed")
            } else {
                codec.encode(samples, frames)
            }
        }),
        MEDIA_BACKEND_ERROR
    );
    assert_eq!(attempts, 2);
    assert_eq!(encoder.state(), "failed");
    assert_eq!(encoder.queued(), 1);
    assert_eq!(encoder.buffered_frames(), 0);
    assert_eq!(encoder.padded_frames, 80);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0].sequence, 0);
    assert_eq!(sink.packets[0].frames, 480);
    assert_eq!(sink.packets[0].flags, 0);
    assert_eq!(
        unsafe { encoder.receive(sink.native()) },
        MEDIA_BACKEND_ERROR
    );

    encoder.recover().unwrap();
    assert_eq!(encoder.discarded_frames, 0);
    assert_eq!(encoder.padded_frames, 80);
    assert_eq!(
        unsafe { encoder.submit(&input(&full, 1, PCM_I16, 100_000, 20)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[1].sequence, 1);
    assert_eq!(sink.packets[1].timestamp, 100_000);
    assert_eq!(sink.packets[1].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(encoder.drain(), STATUS_OK);
    assert!(encoder.padded_frames > 80);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn queue_backpressure_and_invalid_input_accept_no_samples_or_continuity_changes() {
    let mut encoder = stream(1, 5);
    let bytes = vec![0u8; 240 * 2];
    for index in 0..MAX_PACKETS {
        let frame = input(&bytes, 1, PCM_I16, index as u64 * 5000, index as u64);
        assert_eq!(unsafe { encoder.submit(&frame) }, STATUS_OK);
    }
    let next = input(
        &bytes,
        1,
        PCM_I16,
        MAX_PACKETS as u64 * 5000,
        MAX_PACKETS as u64,
    );
    assert_eq!(unsafe { encoder.submit(&next) }, MEDIA_BUSY);
    assert_eq!(encoder.drain(), MEDIA_BUSY);
    assert!(!encoder.draining);
    let mut sink = Sink::new();
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(unsafe { encoder.submit(&next) }, STATUS_OK);
    assert_eq!(encoder.accepted_frames, (MAX_PACKETS as u64 + 1) * 240);
    assert_eq!(
        unsafe {
            encoder.submit(&AudioFrame {
                sample_rate: 44_100,
                ..next
            })
        },
        MEDIA_UNSUPPORTED
    );
    assert_eq!(
        unsafe { encoder.submit(&AudioFrame { len: 1, ..next }) },
        STATUS_INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe {
            encoder.submit(&AudioFrame {
                timestamp_us: u64::MAX,
                flags: AUDIO_DISCONTINUITY,
                ..next
            })
        },
        STATUS_INVALID_ARGUMENT
    );
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    let invalid = f32::NAN.to_le_bytes();
    let bad = input(
        &invalid,
        1,
        PCM_F32,
        (MAX_PACKETS as u64 + 1) * 5000,
        MAX_PACKETS as u64 + 1,
    );
    assert_eq!(unsafe { encoder.submit(&bad) }, STATUS_INVALID_ARGUMENT);
    assert_eq!(encoder.buffered_frames(), 0);
}

#[test]
fn explicit_discontinuity_resets_codec_and_marks_next_packet_without_combining_old_partial() {
    let mut encoder = stream(1, 10);
    let initial = vec![0u8; 480 * 2];
    assert_eq!(
        unsafe { encoder.submit(&input(&initial, 1, PCM_I16, 0, 0)) },
        STATUS_OK
    );
    let partial = vec![0u8; 120 * 2];
    assert_eq!(
        unsafe { encoder.submit(&input(&partial, 1, PCM_I16, 10_000, 1)) },
        STATUS_OK
    );
    let bytes = vec![0u8; 480 * 2];
    let reset = AudioFrame {
        flags: AUDIO_DISCONTINUITY | AUDIO_SILENT | AUDIO_TIMESTAMP_ESTIMATED,
        ..input(&bytes, 1, PCM_I16, 200_000, 20)
    };
    assert_eq!(
        unsafe { encoder.submit(&AudioFrame { flags: 0, ..reset }) },
        STATUS_INVALID_ARGUMENT
    );
    assert_eq!(unsafe { encoder.submit(&reset) }, STATUS_OK);
    assert_eq!(encoder.discarded_frames, 120);
    let mut sink = Sink::new();
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0].timestamp, 0);
    assert_eq!(sink.packets[0].flags, 0);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[1].timestamp, 200_000);
    assert_eq!(sink.packets[1].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(sink.packets[1].sequence, 1);
    assert_eq!(encoder.buffered_frames(), 0);
}

#[test]
fn little_endian_pcm_clips_finite_overrange_and_rejects_nonfinite_samples() {
    let bytes: Vec<u8> = [-2.5f32, -0.25, 0.25, 2.5]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    let frame = input(&bytes, 1, PCM_F32, 0, 0);
    assert_eq!(
        decode_pcm(&bytes, &frame).unwrap(),
        vec![-1.0, -0.25, 0.25, 1.0]
    );
    let bytes = [0x00, 0x80, 0xff, 0x7f];
    assert_eq!(
        decode_pcm(&bytes, &input(&bytes, 1, PCM_I16, 0, 0)).unwrap(),
        vec![-1.0, 32767.0 / 32768.0]
    );
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let bytes = value.to_le_bytes();
        assert_eq!(
            decode_pcm(&bytes, &input(&bytes, 1, PCM_F32, 0, 0)),
            Err(STATUS_INVALID_ARGUMENT)
        );
    }
}

#[test]
fn capture_clock_jitter_drift_and_estimates_preserve_a_stable_output_sample_clock() {
    let mut encoder = stream(1, 5);
    let bytes = vec![0u8; 240 * 2];
    let mut sink = Sink::new();
    // Relative QPC/estimated timestamps fluctuate around an increasingly drifting
    // sample clock, while remaining monotonic as real contiguous capture does.
    let capture_times = [1000, 6020, 11_008, 16_050, 21_035, 26_070];
    for (sequence, capture_time) in capture_times.into_iter().enumerate() {
        let frame = AudioFrame {
            flags: if sequence % 2 == 0 {
                AUDIO_TIMESTAMP_ESTIMATED
            } else {
                0
            },
            ..input(&bytes, 1, PCM_I16, capture_time, sequence as u64)
        };
        // SAFETY: input bytes and sink are local and live for each callback.
        assert_eq!(unsafe { encoder.submit(&frame) }, STATUS_OK);
        assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
        assert_eq!(
            sink.packets[sequence].timestamp,
            1000 + sequence as u64 * 5000
        );
        assert_eq!(sink.packets[sequence].flags, 0);
        assert_eq!(encoder.last_input_timestamp_us, Some(capture_time));
        assert_eq!(
            encoder.input_clock_drift_us,
            capture_time as i64 - (1000 + sequence as i64 * 5000)
        );
    }
    for (sequence, timestamp) in [(5, 31_080), (7, 31_080), (6, 26_069)] {
        let invalid = input(&bytes, 1, PCM_I16, timestamp, sequence);
        assert_eq!(unsafe { encoder.submit(&invalid) }, STATUS_INVALID_ARGUMENT);
        assert_eq!(encoder.last_input_timestamp_us, Some(26_070));
        assert_eq!(encoder.input_clock_drift_us, 70);
        assert_eq!(encoder.queued(), 0);
    }
    // A repeated timestamp is valid for an estimated capture clock; it does not
    // duplicate PCM, because sequence numbers remain contiguous.
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 26_070, 6)) },
        STATUS_OK
    );
    assert_eq!(encoder.input_clock_drift_us, -4930);
    let reset = AudioFrame {
        flags: AUDIO_DISCONTINUITY,
        ..input(&bytes, 1, PCM_I16, 0, 0)
    };
    assert_eq!(unsafe { encoder.submit(&reset) }, STATUS_OK);
    assert_eq!(encoder.last_input_timestamp_us, Some(0));
    assert_eq!(encoder.input_clock_drift_us, 0);
}

#[test]
fn bitrate_update_waits_for_packet_boundary_and_keeps_decoder_continuous() {
    let mut encoder = stream(1, 10);
    let bytes = vec![0u8; 480 * 2];
    let mut sink = Sink::new();
    let mut decoder = Decoder::new(1);
    let first_size = encoder.packet_bytes();
    let first = input(&bytes, 1, PCM_I16, 1000, 0);
    // SAFETY: local PCM and sink allocations live through synchronous calls.
    assert_eq!(unsafe { encoder.submit(&first) }, STATUS_OK);
    assert_eq!(encoder.update_bitrate(64_000), Err("stream_busy"));
    assert_eq!(encoder.configuration.bitrate, 128_000);
    assert_eq!(encoder.packet_bytes(), first_size);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(decoder.decode(&sink.packets[0], 1).len(), 480);

    let partial = input(&bytes[..240 * 2], 1, PCM_I16, 11_000, 1);
    assert_eq!(unsafe { encoder.submit(&partial) }, STATUS_OK);
    assert_eq!(encoder.update_bitrate(64_000), Err("stream_busy"));
    let continuation = input(&bytes[..240 * 2], 1, PCM_I16, 16_000, 2);
    assert_eq!(unsafe { encoder.submit(&continuation) }, STATUS_OK);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(decoder.decode(&sink.packets[1], 1).len(), 480);

    assert_eq!(encoder.update_bitrate(5999), Err("unsupported_bitrate"));
    assert_eq!(encoder.update_bitrate(64_000), Ok(()));
    let updated_size = encoder.packet_bytes();
    assert_ne!(updated_size, first_size);
    assert_eq!(encoder.update_bitrate(64_000), Ok(()));
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 21_000, 3)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[2].bytes.len(), updated_size);
    assert_eq!(sink.packets[2].flags, 0);
    assert_eq!(sink.packets[2].timestamp, 21_000);
    assert_eq!(sink.packets[2].sequence, 2);
    assert_eq!(decoder.decode(&sink.packets[2], 1).len(), 480);
    assert_eq!(encoder.drain(), STATUS_OK);
    assert_eq!(encoder.update_bitrate(96_000), Err("stream_closed"));
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn update_before_first_input_does_not_report_a_false_discontinuity() {
    let mut encoder = stream(2, 5);
    assert_eq!(encoder.update_bitrate(96_000), Ok(()));
    let bytes = vec![0u8; 240 * 2 * 2];
    let mut sink = Sink::new();
    // SAFETY: local bytes and sink live through synchronous calls.
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 2, PCM_I16, 0, 0)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0].flags, 0);
    assert_eq!(sink.packets[0].bytes.len(), encoder.packet_bytes());
}

#[test]
fn failed_bitrate_premeasure_keeps_live_epoch_until_update_and_recovery_complete() {
    let mut encoder = stream(1, 10);
    let bytes = vec![0u8; 480 * 2];
    let original_size = encoder.packet_bytes();
    let mut sink = Sink::new();

    // SAFETY: the local PCM and sink allocations live through synchronous calls.
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 1_000, 0)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(
        encoder.update_bitrate_with(64_000, |_| Err("codec_initialization_failed")),
        Err("codec_initialization_failed")
    );
    assert_eq!(encoder.configuration.bitrate, 128_000);
    assert_eq!(encoder.packet_bytes(), original_size);
    assert_eq!(encoder.state(), "encoding");

    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 11_000, 1)) },
        STATUS_OK
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[1].bytes.len(), original_size);
    assert_eq!(sink.packets[1].flags, 0);

    encoder.update_bitrate(64_000).unwrap();
    let updated_size = encoder.packet_bytes();
    assert_ne!(updated_size, original_size);
    encoder.failure = Some("codec_encode_failed");
    assert_eq!(encoder.drain(), MEDIA_BACKEND_ERROR);
    encoder.recover().unwrap();
    assert_eq!(encoder.configuration.bitrate, 64_000);
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 1, PCM_I16, 21_000, 2)) },
        STATUS_OK
    );
    assert_eq!(encoder.drain(), STATUS_OK);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(sink.packets[2].bytes.len(), updated_size);
    assert_eq!(sink.packets[2].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn high_quality_stereo_packets_survive_rejection_recovery_and_drain() {
    let mut encoder = Stream::new(
        "test".into(),
        Configuration {
            channels: 2,
            packet_ms: 20,
            bitrate: 512_000,
        },
    )
    .unwrap();
    assert!(encoder.packet_bytes() > 0);
    let bytes = vec![0u8; 960 * 2 * 2];
    let mut sink = Sink::new();
    // SAFETY: PCM and sink allocations remain live through each synchronous call.
    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 2, PCM_I16, 10_000, 0)) },
        STATUS_OK
    );
    assert_eq!(encoder.queued(), 1);
    sink.reject = true;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
    assert_eq!(encoder.queued(), 1);
    encoder.failure = Some("codec_encode_failed");
    assert_eq!(encoder.drain(), MEDIA_BACKEND_ERROR);
    encoder.recover().unwrap();
    assert_eq!(encoder.queued(), 1);
    sink.reject = false;
    assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
    assert_eq!(sink.packets[0], sink.packets[1]);
    assert_eq!(sink.packets[1].bytes.len(), encoder.packet_bytes());
    assert_eq!(sink.packets[1].flags, 0);
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_PENDING);

    assert_eq!(
        unsafe { encoder.submit(&input(&bytes, 2, PCM_I16, 100_000, 40)) },
        STATUS_OK
    );
    assert_eq!(encoder.drain(), STATUS_OK);
    while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
    assert_eq!(sink.packets[2].sequence, 1);
    assert_eq!(sink.packets[2].timestamp, 100_000);
    assert_eq!(sink.packets[2].flags, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(sink.packets[2].bytes.len(), encoder.packet_bytes());
    assert_eq!(
        sink.packets.last().unwrap().flags,
        AUDIO_PACKET_END_OF_STREAM
    );
    assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
}

#[test]
fn surround_packets_retain_queue_order_across_recovery_and_end_once() {
    for (channels, bitrate) in [(6, 1_536_000), (8, 2_048_000)] {
        let configuration = Configuration {
            channels,
            packet_ms: 10,
            bitrate,
        };
        let mut encoder = Stream::new("surround".into(), configuration).unwrap();
        let samples = vec![0u8; 480 * channels as usize * 2];
        let mut sink = Sink::new();
        // SAFETY: input and sink allocations remain live through each call.
        assert_eq!(
            unsafe { encoder.submit(&input(&samples, channels, PCM_I16, 1_000, 0)) },
            STATUS_OK
        );
        assert_eq!(encoder.queued(), 1);
        sink.reject = true;
        assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_BUSY);
        encoder.failure = Some("codec_encode_failed");
        encoder.recover().unwrap();
        sink.reject = false;
        assert_eq!(unsafe { encoder.receive(sink.native()) }, STATUS_OK);
        assert_eq!(sink.packets[0], sink.packets[1]);
        assert_eq!(sink.packets[1].bytes.len(), encoder.packet_bytes());
        assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_PENDING);

        assert_eq!(
            unsafe { encoder.submit(&input(&samples, channels, PCM_I16, 100_000, 50)) },
            STATUS_OK
        );
        assert_eq!(encoder.drain(), STATUS_OK);
        while unsafe { encoder.receive(sink.native()) } == STATUS_OK {}
        assert_eq!(sink.packets[2].sequence, 1);
        assert_eq!(sink.packets[2].timestamp, 100_000);
        assert_eq!(sink.packets[2].flags, AUDIO_PACKET_DISCONTINUITY);
        assert_eq!(sink.packets[2].bytes.len(), encoder.packet_bytes());
        assert_eq!(
            sink.packets.last().unwrap().flags,
            AUDIO_PACKET_END_OF_STREAM
        );
        assert_eq!(unsafe { encoder.receive(sink.native()) }, MEDIA_CLOSED);
    }
}
