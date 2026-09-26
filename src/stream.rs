use crate::{codec::Encoder, model::*};
use amaterasu_media::*;
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use std::collections::VecDeque;

struct Packet {
    bytes: Vec<u8>,
    frames: u32,
    flags: u32,
    timestamp: u64,
    sequence: u64,
}

pub struct Stream {
    pub owner: String,
    pub configuration: Configuration,
    encoder: Encoder,
    samples: Vec<f32>,
    packets: VecDeque<Packet>,
    origin: Option<u64>,
    accepted_frames: u64,
    encoded_frames: u64,
    input_sequence: Option<u64>,
    output_sequence: u64,
    discontinuity_pending: bool,
    draining: bool,
    eos_delivered: bool,
    pub failure: Option<&'static str>,
    pub discarded_frames: u64,
    pub padded_frames: u64,
    pub last_input_timestamp_us: Option<u64>,
    pub input_clock_drift_us: i64,
}

impl Stream {
    pub fn new(owner: String, configuration: Configuration) -> EncodeResult<Self> {
        Ok(Self {
            owner,
            configuration,
            encoder: Encoder::new(configuration)?,
            samples: Vec::new(),
            packets: VecDeque::new(),
            origin: None,
            accepted_frames: 0,
            encoded_frames: 0,
            input_sequence: None,
            output_sequence: 0,
            discontinuity_pending: false,
            draining: false,
            eos_delivered: false,
            failure: None,
            discarded_frames: 0,
            padded_frames: 0,
            last_input_timestamp_us: None,
            input_clock_drift_us: 0,
        })
    }

    pub fn lookahead(&self) -> u32 {
        self.encoder.lookahead
    }
    pub fn packet_bytes(&self) -> usize {
        self.encoder.packet_bytes
    }

    pub fn queued(&self) -> usize {
        self.packets.len()
    }

    pub fn buffered_frames(&self) -> usize {
        self.samples.len() / self.configuration.channels as usize
    }

    pub fn state(&self) -> &'static str {
        if self.failure.is_some() {
            "failed"
        } else if self.eos_delivered {
            "ended"
        } else if self.draining {
            "draining"
        } else {
            "encoding"
        }
    }

    pub fn update_bitrate(&mut self, bitrate: u32) -> EncodeResult<()> {
        let configuration = Configuration {
            bitrate,
            ..self.configuration
        }
        .validate()?;
        if self.failure.is_some() {
            return Err("stream_failed");
        }
        if self.draining {
            return Err("stream_closed");
        }
        if configuration == self.configuration {
            return Ok(());
        }
        if !self.packets.is_empty() || !self.samples.is_empty() {
            return Err("stream_busy");
        }
        // Build and measure the replacement before changing the live stream. A
        // decoder must reset for the new Opus epoch and packet byte length.
        let encoder = Encoder::new(configuration)?;
        self.encoder = encoder;
        self.configuration = configuration;
        self.discontinuity_pending = self.origin.is_some();
        Ok(())
    }

    pub fn recover(&mut self) -> EncodeResult<()> {
        self.recover_with(Encoder::new)
    }

    fn recover_with(
        &mut self,
        build_encoder: impl FnOnce(Configuration) -> EncodeResult<Encoder>,
    ) -> EncodeResult<()> {
        if self.failure.is_none() {
            return Err("stream_not_failed");
        }
        // Construct a complete replacement before discarding any state. If
        // codec initialization fails, the failed stream stays inspectable.
        let encoder = build_encoder(self.configuration)?;
        self.encoder = encoder;
        self.discarded_frames += self.buffered_frames() as u64;
        self.samples.clear();
        // Packets already produced by the old encoder are valid even if the
        // caller recovers before receiving them. Only an EOS marker from a
        // failed drain belongs to the abandoned epoch.
        self.packets
            .retain(|packet| packet.flags & AUDIO_PACKET_END_OF_STREAM == 0);
        self.origin = None;
        self.accepted_frames = 0;
        self.encoded_frames = 0;
        self.input_sequence = None;
        self.discontinuity_pending = self.output_sequence != 0;
        self.draining = false;
        self.eos_delivered = false;
        self.failure = None;
        self.last_input_timestamp_us = None;
        self.input_clock_drift_us = 0;
        Ok(())
    }

    /// # Safety
    /// The validated frame's sample allocation remains initialized and immutable
    /// for this call. Samples are copied and never retained as caller pointers.
    pub unsafe fn submit(&mut self, frame: &AudioFrame) -> Status {
        if self.failure.is_some() {
            return MEDIA_BACKEND_ERROR;
        }
        if self.draining {
            return MEDIA_CLOSED;
        }
        if let Err(status) = frame.validate() {
            return status;
        }
        if frame.sample_rate != SAMPLE_RATE || frame.channels != self.configuration.channels {
            return MEDIA_UNSUPPORTED;
        }
        let discontinuity = frame.flags & AUDIO_DISCONTINUITY != 0;
        if !self.valid_timing(frame, discontinuity) {
            return STATUS_INVALID_ARGUMENT;
        }
        let pending = if discontinuity {
            0
        } else {
            self.buffered_frames()
        };
        let new_packets = (pending + frame.frames as usize) / self.configuration.packet_frames();
        if self.packets.len() + new_packets > MAX_PACKETS {
            return MEDIA_BUSY;
        }
        // SAFETY: allocation lifetime is the submit caller's obligation.
        let samples = match decode_pcm(unsafe { frame.bytes().unwrap() }, frame) {
            Ok(samples) => samples,
            Err(status) => return status,
        };
        if discontinuity {
            if let Err(error) = self.encoder.reset() {
                return self.fail(error);
            }
            self.discarded_frames += self.buffered_frames() as u64;
            self.samples.clear();
            self.origin = None;
            self.accepted_frames = 0;
            self.encoded_frames = 0;
            self.discontinuity_pending = true;
        }
        let origin = *self.origin.get_or_insert(frame.timestamp_us);
        let expected =
            timestamp(origin, self.accepted_frames).expect("timing validated before acceptance");
        self.input_clock_drift_us = (i128::from(frame.timestamp_us) - i128::from(expected))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64;
        self.last_input_timestamp_us = Some(frame.timestamp_us);
        self.accepted_frames += u64::from(frame.frames);
        self.input_sequence = Some(frame.sequence);
        self.samples.extend_from_slice(&samples);
        match self.encode_available() {
            Ok(()) => STATUS_OK,
            Err(error) => self.fail(error),
        }
    }

    fn valid_timing(&self, frame: &AudioFrame, discontinuity: bool) -> bool {
        let accepted = if discontinuity {
            0
        } else {
            self.accepted_frames
        };
        let origin = if discontinuity {
            frame.timestamp_us
        } else {
            self.origin.unwrap_or(frame.timestamp_us)
        };
        let Some(total) = accepted.checked_add(u64::from(frame.frames)) else {
            return false;
        };
        // Reserve enough timestamp/sequence range for bounded drain padding too.
        if timestamp(origin, total.saturating_add(1920)).is_none()
            || self.output_sequence > u64::MAX - 32
        {
            return false;
        }
        if discontinuity || self.origin.is_none() {
            return true;
        }
        self.input_sequence.and_then(|last| last.checked_add(1)) == Some(frame.sequence)
            && self
                .last_input_timestamp_us
                .is_some_and(|previous| frame.timestamp_us >= previous)
    }

    fn encode_available(&mut self) -> EncodeResult<()> {
        let frames = self.configuration.packet_frames();
        let samples = frames * self.configuration.channels as usize;
        while self.samples.len() >= samples {
            let bytes = self.encoder.encode(&self.samples[..samples], frames)?;
            self.samples.drain(..samples);
            self.packets.push_back(Packet {
                bytes,
                frames: frames as u32,
                flags: if self.discontinuity_pending {
                    AUDIO_PACKET_DISCONTINUITY
                } else {
                    0
                },
                timestamp: timestamp(self.origin.unwrap_or(0), self.encoded_frames)
                    .ok_or("timestamp_limit")?,
                sequence: self.output_sequence,
            });
            self.discontinuity_pending = false;
            self.output_sequence += 1;
            self.encoded_frames += frames as u64;
        }
        Ok(())
    }

    pub fn drain(&mut self) -> Status {
        if self.failure.is_some() {
            return MEDIA_BACKEND_ERROR;
        }
        if self.draining {
            return STATUS_OK;
        }
        let frames = self.configuration.packet_frames();
        let pending = self.buffered_frames();
        let needed = if self.origin.is_some() {
            (pending + self.encoder.lookahead as usize).div_ceil(frames)
        } else {
            0
        };
        if self.packets.len() + needed + 1 > MAX_PACKETS {
            return MEDIA_BUSY;
        }
        self.padded_frames = (needed * frames - pending) as u64;
        self.samples
            .resize(needed * frames * self.configuration.channels as usize, 0.0);
        if let Err(error) = self.encode_available() {
            return self.fail(error);
        }
        self.packets.push_back(Packet {
            bytes: Vec::new(),
            frames: 0,
            flags: AUDIO_PACKET_END_OF_STREAM,
            timestamp: timestamp(self.origin.unwrap_or(0), self.encoded_frames).unwrap_or(0),
            sequence: self.output_sequence,
        });
        self.draining = true;
        STATUS_OK
    }

    /// # Safety
    /// The sink callback/context remain live through synchronous delivery and
    /// cannot retain the borrowed packet allocation or unwind across the ABI.
    pub unsafe fn receive(&mut self, sink: AudioPacketSink) -> Status {
        // A codec failure cannot invalidate packets already encoded. Deliver
        // them in order before reporting the failure once the queue is empty.
        // A failed drain must never turn an already queued EOS into success.
        if self.failure.is_some()
            && self
                .packets
                .front()
                .is_none_or(|packet| packet.flags & AUDIO_PACKET_END_OF_STREAM != 0)
        {
            return MEDIA_BACKEND_ERROR;
        }
        let Some(packet) = self.packets.front() else {
            return if self.eos_delivered {
                MEDIA_CLOSED
            } else {
                MEDIA_PENDING
            };
        };
        let view = EncodedAudioPacket {
            codec: CODEC_OPUS,
            flags: packet.flags,
            sample_rate: SAMPLE_RATE,
            channels: self.configuration.channels,
            frames: packet.frames,
            data: packet.bytes.as_ptr(),
            len: packet.bytes.len(),
            timestamp_us: packet.timestamp,
            sequence: packet.sequence,
        };
        // SAFETY: the queue owns bytes until this callback returns; rejection
        // leaves both packet contents and delivery sequence unchanged.
        let result = unsafe { sink.send(&view) };
        if result == STATUS_OK {
            self.eos_delivered = packet.flags & AUDIO_PACKET_END_OF_STREAM != 0;
            self.packets.pop_front();
        }
        result
    }

    fn fail(&mut self, error: &'static str) -> Status {
        self.failure = Some(error);
        MEDIA_BACKEND_ERROR
    }
}

fn timestamp(origin: u64, frames: u64) -> Option<u64> {
    let offset = u128::from(frames) * 1_000_000 / u128::from(SAMPLE_RATE);
    origin.checked_add(u64::try_from(offset).ok()?)
}

fn decode_pcm(bytes: &[u8], frame: &AudioFrame) -> Result<Vec<f32>, Status> {
    let count = frame.frames as usize * frame.channels as usize;
    if frame.flags & AUDIO_SILENT != 0 {
        return Ok(vec![0.0; count]);
    }
    let mut samples = match frame.format {
        PCM_F32 => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| f32::from_le_bytes(*sample))
            .collect::<Vec<_>>(),
        PCM_I16 => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| f32::from(i16::from_le_bytes(*sample)) / 32768.0)
            .collect(),
        _ => return Err(MEDIA_UNSUPPORTED),
    };
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    samples
        .iter_mut()
        .for_each(|sample| *sample = sample.clamp(-1.0, 1.0));
    Ok(samples)
}

#[cfg(test)]
mod tests;
