use crate::{model::*, stream::Stream};
use amaterasu_media::{AudioFrame, AudioPacketSink, MEDIA_CLOSED};
use amaterasu_module_sdk::abi::Status;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub struct ControlReply {
    pub payload: Value,
    pub created: Option<u64>,
}

struct Record {
    stream: u64,
    configuration: Configuration,
}

pub struct Controller {
    enabled: bool,
    streams: BTreeMap<u64, Stream>,
    operations: BTreeMap<(String, String), Record>,
    issued: BTreeSet<u64>,
    closed: BTreeMap<u64, String>,
}

impl Controller {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            streams: BTreeMap::new(),
            operations: BTreeMap::new(),
            issued: BTreeSet::new(),
            closed: BTreeMap::new(),
        }
    }

    pub fn handle(&mut self, owner: &str, operation: Operation) -> EncodeResult<ControlReply> {
        let (payload, created) = match operation {
            Operation::Enumerate => (self.enumerate(), None),
            Operation::Open {
                channels,
                packet_ms,
                bitrate,
                operation_id,
            } => self.open(
                owner,
                Configuration {
                    channels,
                    packet_ms,
                    bitrate,
                },
                operation_id,
            )?,
            Operation::Lookup { operation_id } => {
                validate_operation(&operation_id)?;
                let record = self
                    .operations
                    .get(&(owner.into(), operation_id))
                    .ok_or("stream_not_found")?;
                (
                    self.open_reply(record.stream, self.owned(owner, record.stream)?),
                    None,
                )
            }
            Operation::Update { stream, bitrate } => (self.update(owner, stream, bitrate)?, None),
            Operation::Recover { stream, bitrate } => (self.recover(owner, stream, bitrate)?, None),
            Operation::Close { stream } => (self.close(owner, stream)?, None),
            Operation::Status { stream } => (self.status(owner, stream), None),
        };
        Ok(ControlReply { payload, created })
    }

    fn open(
        &mut self,
        owner: &str,
        configuration: Configuration,
        operation: Option<String>,
    ) -> EncodeResult<(Value, Option<u64>)> {
        if !self.enabled {
            return Err("disabled");
        }
        let configuration = configuration.validate()?;
        if let Some(operation) = &operation {
            validate_operation(operation)?;
            if let Some(record) = self.operations.get(&(owner.into(), operation.clone())) {
                if record.configuration != configuration {
                    return Err("operation_conflict");
                }
                let stream = self.streams.get(&record.stream).ok_or("stream_closed")?;
                return Ok((self.open_reply(record.stream, stream), None));
            }
            if self.operations.len() >= 128 {
                return Err("operation_limit");
            }
        }
        if self.streams.len() >= 4
            || self
                .streams
                .values()
                .filter(|stream| stream.owner == owner)
                .count()
                >= 2
        {
            return Err("stream_limit");
        }
        if self.issued.len() >= 1024 {
            return Err("stream_history_limit");
        }
        let id = self.random_id()?;
        let stream = Stream::new(owner.into(), configuration)?;
        let reply = self.open_reply(id, &stream);
        self.issued.insert(id);
        self.streams.insert(id, stream);
        if let Some(operation) = operation {
            self.operations.insert(
                (owner.into(), operation),
                Record {
                    stream: id,
                    configuration,
                },
            );
        }
        Ok((reply, Some(id)))
    }

    fn random_id(&self) -> EncodeResult<u64> {
        for _ in 0..16 {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).map_err(|_| "randomness_unavailable")?;
            let id = u64::from_ne_bytes(bytes);
            if id != 0 && !self.issued.contains(&id) {
                return Ok(id);
            }
        }
        Err("randomness_unavailable")
    }

    fn owned(&self, owner: &str, id: u64) -> EncodeResult<&Stream> {
        self.streams
            .get(&id)
            .filter(|stream| stream.owner == owner)
            .ok_or("stream_not_found")
    }

    fn owned_mut(&mut self, owner: &str, id: u64) -> Option<&mut Stream> {
        self.streams
            .get_mut(&id)
            .filter(|stream| stream.owner == owner)
    }

    pub fn close(&mut self, owner: &str, id: u64) -> EncodeResult<Value> {
        if self.owned(owner, id).is_ok() {
            self.streams.remove(&id);
            self.closed.insert(id, owner.into());
        } else if self
            .closed
            .get(&id)
            .is_none_or(|closed_owner| closed_owner != owner)
        {
            return Err("stream_not_found");
        }
        Ok(json!({"ok":true}))
    }

    // Only a failed Complete may call this: a published close must retain its
    // operation tombstone and issued ID so a replay cannot create another stream.
    pub fn rollback_unpublished_open(&mut self, owner: &str, id: u64) {
        if self.owned(owner, id).is_err() {
            return;
        }
        self.streams.remove(&id);
        self.issued.remove(&id);
        self.operations
            .retain(|(record_owner, _), record| record_owner != owner || record.stream != id);
    }

    fn update(&mut self, owner: &str, id: u64, bitrate: u32) -> EncodeResult<Value> {
        let stream = self.owned_mut(owner, id).ok_or("stream_not_found")?;
        stream.update_bitrate(bitrate)?;
        Ok(json!({
            "ok": true,
            "stream": id,
            "bitrate": stream.configuration.bitrate,
            "packet_bytes": stream.packet_bytes()
        }))
    }

    fn recover(&mut self, owner: &str, id: u64, bitrate: Option<u32>) -> EncodeResult<Value> {
        let stream = self.owned_mut(owner, id).ok_or("stream_not_found")?;
        match bitrate {
            Some(bitrate) => stream.recover_with_bitrate(Some(bitrate))?,
            None => stream.recover()?,
        }
        Ok(
            json!({"ok": true, "stream": id, "bitrate": stream.configuration.bitrate, "packet_bytes": stream.packet_bytes()}),
        )
    }

    fn enumerate(&self) -> Value {
        let encoders = if self.enabled {
            [
                Configuration {
                    channels: 1,
                    ..Configuration::default()
                },
                Configuration {
                    channels: 2,
                    ..Configuration::default()
                },
                Configuration {
                    channels: 6,
                    ..Configuration::default()
                },
                Configuration {
                    channels: 8,
                    ..Configuration::default()
                },
            ]
            .into_iter()
            .map(|configuration| {
                let (bitrate_min, bitrate_max) = configuration.bitrate_range();
                let layout = configuration.layout().expect("advertised layout exists");
                json!({
                    "codec": "opus",
                    "sample_rate": SAMPLE_RATE,
                    "channels": [configuration.channels],
                    "packet_ms": [5, 10, 20],
                    "bitrate_min": bitrate_min,
                    "bitrate_max": bitrate_max,
                    "bitrate_min_5ms": Configuration { packet_ms: 5, ..configuration }.bitrate_range().0,
                    "streams": layout.streams,
                    "coupled_streams": layout.coupled_streams,
                    "channel_mapping": layout.mapping,
                    "wave_channel_mapping": layout.wave_mapping,
                    "mapping_family": if configuration.channels <= 2 { 0 } else { 1 },
                    "pcm_formats": ["f32", "i16"],
                    "constant_bitrate": true
                })
            })
            .collect()
        } else {
            Vec::new()
        };
        json!({"ok": true, "enabled": self.enabled, "encoders": encoders})
    }

    fn open_reply(&self, id: u64, stream: &Stream) -> Value {
        let layout = stream
            .configuration
            .layout()
            .expect("open stream has a valid layout");
        json!({
            "ok": true,
            "stream": id,
            "codec": "opus",
            "sample_rate": SAMPLE_RATE,
            "channels": stream.configuration.channels,
            "streams": layout.streams,
            "coupled_streams": layout.coupled_streams,
            "channel_mapping": layout.mapping,
            "wave_channel_mapping": layout.wave_mapping,
            "mapping_family": if stream.configuration.channels <= 2 { 0 } else { 1 },
            "packet_ms": stream.configuration.packet_ms,
            "bitrate": stream.configuration.bitrate,
            "lookahead_frames": stream.lookahead(),
            "packet_bytes": stream.packet_bytes(),
            "constant_bitrate": true
        })
    }

    fn status(&self, owner: &str, requested: Option<u64>) -> Value {
        let streams: Vec<_> = self
            .streams
            .iter()
            .filter(|(id, stream)| {
                stream.owner == owner && requested.is_none_or(|wanted| wanted == **id)
            })
            .map(|(id, stream)| self.stream_status(*id, stream))
            .collect();
        json!({"ok": true, "enabled": self.enabled, "streams": streams})
    }

    fn stream_status(&self, id: u64, stream: &Stream) -> Value {
        let mut value = self.open_reply(id, stream);
        let fields = value.as_object_mut().expect("open reply is an object");
        fields.remove("ok");
        fields.insert("state".into(), json!(stream.state()));
        fields.insert("failure".into(), json!(stream.failure));
        fields.insert("queued_packets".into(), json!(stream.queued()));
        fields.insert("buffered_frames".into(), json!(stream.buffered_frames()));
        fields.insert("discarded_frames".into(), json!(stream.discarded_frames));
        fields.insert("padded_frames".into(), json!(stream.padded_frames));
        fields.insert(
            "last_input_timestamp_us".into(),
            json!(stream.last_input_timestamp_us),
        );
        fields.insert(
            "input_clock_drift_us".into(),
            json!(stream.input_clock_drift_us),
        );
        value
    }

    pub fn shutdown(&mut self) {
        self.enabled = false;
        self.streams.clear();
    }

    pub fn is_shutdown(&self) -> bool {
        !self.enabled
    }

    /// # Safety
    /// The frame allocation remains valid for the synchronous submit call.
    pub unsafe fn submit(&mut self, owner: &str, id: u64, frame: &AudioFrame) -> Status {
        match self.owned_mut(owner, id) {
            Some(stream) => unsafe { stream.submit(frame) },
            None => MEDIA_CLOSED,
        }
    }

    /// # Safety
    /// Sink callback/context are live; borrowed bytes cannot escape its callback.
    pub unsafe fn receive(&mut self, owner: &str, id: u64, sink: AudioPacketSink) -> Status {
        match self.owned_mut(owner, id) {
            Some(stream) => unsafe { stream.receive(sink) },
            None => MEDIA_CLOSED,
        }
    }

    pub fn drain(&mut self, owner: &str, id: u64) -> Status {
        self.owned_mut(owner, id)
            .map_or(MEDIA_CLOSED, Stream::drain)
    }
}

#[cfg(test)]
mod tests;
