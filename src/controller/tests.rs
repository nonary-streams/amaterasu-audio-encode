use super::*;
use amaterasu_media::{
    AUDIO_PACKET_DISCONTINUITY, AUDIO_PACKET_END_OF_STREAM, EncodedAudioPacket,
    MEDIA_BACKEND_ERROR, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_PENDING, PCM_I16,
};
use amaterasu_module_sdk::abi::STATUS_OK;
use std::ffi::c_void;

fn open(id: Option<&str>) -> Operation {
    Operation::Open {
        channels: 2,
        packet_ms: 10,
        bitrate: 128_000,
        operation_id: id.map(str::to_owned),
    }
}

#[test]
fn disabled_controls_do_not_allocate_and_validation_is_explicit() {
    let mut controller = Controller::new(false);
    assert_eq!(
        controller
            .handle("a", Operation::Enumerate)
            .unwrap()
            .payload["encoders"],
        json!([])
    );
    assert_eq!(controller.handle("a", open(None)).err(), Some("disabled"));
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Lookup {
                    operation_id: "absent".into()
                }
            )
            .err(),
        Some("stream_not_found")
    );
    assert_eq!(
        controller
            .handle("a", Operation::Status { stream: None })
            .unwrap()
            .payload["streams"],
        json!([])
    );
    for (configuration, code) in [
        (
            Configuration {
                channels: 4,
                ..Configuration::default()
            },
            "unsupported_channels",
        ),
        (
            Configuration {
                packet_ms: 2,
                ..Configuration::default()
            },
            "unsupported_packet_duration",
        ),
        (
            Configuration {
                bitrate: 5999,
                ..Configuration::default()
            },
            "unsupported_bitrate",
        ),
    ] {
        assert_eq!(configuration.validate(), Err(code));
    }
    assert!(
        serde_json::from_value::<Operation>(json!({"action":"open","sample_rate":44100})).is_err()
    );
}

#[test]
fn caller_owned_random_sessions_retry_lookup_close_and_limits_are_bounded() {
    let mut controller = Controller::new(true);
    let first = controller.handle("a", open(Some("one"))).unwrap();
    let id = first.created.unwrap();
    assert_ne!(id, 0);
    let retry = controller.handle("a", open(Some("one"))).unwrap();
    assert!(retry.created.is_none());
    assert_eq!(retry.payload, first.payload);
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Lookup {
                    operation_id: "one".into()
                }
            )
            .unwrap()
            .payload,
        first.payload
    );
    assert_eq!(
        controller
            .handle(
                "b",
                Operation::Lookup {
                    operation_id: "one".into()
                }
            )
            .err(),
        Some("stream_not_found")
    );
    assert_eq!(controller.close("b", id), Err("stream_not_found"));
    assert_eq!(
        controller
            .handle("b", Operation::Status { stream: Some(id) })
            .unwrap()
            .payload["streams"],
        json!([])
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Open {
                    channels: 1,
                    packet_ms: 10,
                    bitrate: 128_000,
                    operation_id: Some("one".into())
                }
            )
            .err(),
        Some("operation_conflict")
    );
    controller.handle("a", open(None)).unwrap();
    assert_eq!(
        controller.handle("a", open(None)).err(),
        Some("stream_limit")
    );
    controller.handle("b", open(None)).unwrap();
    controller.handle("c", open(None)).unwrap();
    assert_eq!(
        controller.handle("d", open(None)).err(),
        Some("stream_limit")
    );
    controller.close("a", id).unwrap();
    assert_eq!(
        controller.handle("a", open(Some("one"))).err(),
        Some("stream_closed")
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Lookup {
                    operation_id: "one".into()
                }
            )
            .err(),
        Some("stream_not_found")
    );
    controller.shutdown();
    assert!(controller.streams.is_empty());
}

#[test]
fn bitrate_update_is_owned_validated_and_reflected_in_status_and_lookup() {
    let mut controller = Controller::new(true);
    let id = controller
        .handle("a", open(Some("rate")))
        .unwrap()
        .created
        .unwrap();
    assert_eq!(
        controller
            .handle("a", Operation::Enumerate)
            .unwrap()
            .payload["encoders"][0]["bitrate_max"],
        512_000
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Update {
                    stream: id,
                    bitrate: 512_001,
                },
            )
            .err(),
        Some("unsupported_bitrate")
    );
    let update = Operation::Update {
        stream: id,
        bitrate: 64_000,
    };
    assert_eq!(
        controller.handle("b", update).err(),
        Some("stream_not_found")
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Update {
                    stream: id,
                    bitrate: 5999,
                },
            )
            .err(),
        Some("unsupported_bitrate")
    );
    let reply = controller
        .handle(
            "a",
            Operation::Update {
                stream: id,
                bitrate: 512_000,
            },
        )
        .unwrap();
    assert_eq!(reply.payload["bitrate"], 512_000);
    assert!(reply.payload["packet_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        controller
            .handle("a", Operation::Status { stream: Some(id) })
            .unwrap()
            .payload["streams"][0]["bitrate"],
        512_000
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Lookup {
                    operation_id: "rate".into()
                }
            )
            .unwrap()
            .payload["bitrate"],
        512_000
    );
    assert_eq!(controller.close("a", id).unwrap()["ok"], true);
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Update {
                    stream: id,
                    bitrate: 96_000
                }
            )
            .err(),
        Some("stream_not_found")
    );
}

#[test]
fn surround_open_reports_the_decoder_layout_and_releases_the_stream() {
    let mut controller = Controller::new(true);
    let catalog = controller
        .handle("owner", Operation::Enumerate)
        .unwrap()
        .payload;
    for (channels, streams, coupled, maximum) in [(6, 4, 2, 1_536_000), (8, 5, 3, 2_048_000)] {
        let minimum = if channels == 6 { 256_000 } else { 450_000 };
        assert_eq!(
            (Configuration {
                channels,
                packet_ms: 10,
                bitrate: minimum - 1
            })
            .validate(),
            Err("unsupported_bitrate")
        );
        assert_eq!(
            (Configuration {
                channels,
                packet_ms: 10,
                bitrate: maximum + 1
            })
            .validate(),
            Err("unsupported_bitrate")
        );
        let entry = catalog["encoders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["channels"] == json!([channels]))
            .unwrap();
        assert_eq!(entry["bitrate_max"], maximum);
        assert_eq!(entry["bitrate_min"], minimum);
        assert_eq!(
            entry["bitrate_min_5ms"],
            if channels == 8 { 512_000 } else { minimum }
        );
        if channels == 8 {
            assert_eq!(
                (Configuration {
                    channels,
                    packet_ms: 5,
                    bitrate: 450_000
                })
                .validate(),
                Err("unsupported_bitrate")
            );
        }
        let opened = controller
            .handle(
                "owner",
                Operation::Open {
                    channels,
                    packet_ms: 10,
                    bitrate: maximum,
                    operation_id: None,
                },
            )
            .unwrap();
        let id = opened.created.unwrap();
        assert_eq!(opened.payload["streams"], streams);
        assert_eq!(opened.payload["coupled_streams"], coupled);
        let (vorbis_mapping, wave_mapping) = if channels == 6 {
            (json!([0, 4, 1, 2, 3, 5]), json!([0, 1, 4, 5, 2, 3]))
        } else {
            (
                json!([0, 6, 1, 2, 3, 4, 5, 7]),
                json!([0, 1, 6, 7, 4, 5, 2, 3]),
            )
        };
        assert_eq!(opened.payload["mapping_family"], 1);
        assert_eq!(opened.payload["channel_mapping"], vorbis_mapping);
        assert_eq!(opened.payload["wave_channel_mapping"], wave_mapping);
        assert!(opened.payload["packet_bytes"].as_u64().unwrap() > 0);
        assert_eq!(controller.close("owner", id).unwrap()["ok"], true);
        assert_eq!(controller.drain("owner", id), MEDIA_CLOSED);
    }
}

#[test]
fn only_the_owner_can_recover_a_failed_stream_without_changing_its_identity() {
    let mut controller = Controller::new(true);
    let id = controller
        .handle("a", open(Some("failed")))
        .unwrap()
        .created
        .unwrap();
    let action: Operation =
        serde_json::from_value(json!({"action":"recover","stream":id})).unwrap();
    assert_eq!(
        controller.handle("a", action).err(),
        Some("stream_not_failed")
    );
    assert_eq!(
        controller
            .handle(
                "b",
                Operation::Recover {
                    stream: id,
                    bitrate: None
                }
            )
            .err(),
        Some("stream_not_found")
    );
    controller.streams.get_mut(&id).unwrap().failure = Some("codec_encode_failed");
    assert_eq!(
        controller
            .handle(
                "b",
                Operation::Recover {
                    stream: id,
                    bitrate: None
                }
            )
            .err(),
        Some("stream_not_found")
    );
    assert_eq!(
        controller
            .handle("a", Operation::Status { stream: Some(id) })
            .unwrap()
            .payload["streams"][0]["state"],
        "failed"
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Recover {
                    stream: id,
                    bitrate: Some(5_999)
                }
            )
            .err(),
        Some("unsupported_bitrate")
    );
    assert_eq!(controller.streams.get(&id).unwrap().state(), "failed");
    let reply = controller
        .handle(
            "a",
            Operation::Recover {
                stream: id,
                bitrate: Some(64_000),
            },
        )
        .unwrap();
    assert_eq!(reply.payload["stream"], id);
    assert_eq!(reply.payload["bitrate"], 64_000);
    assert_eq!(
        controller
            .handle("a", Operation::Status { stream: Some(id) })
            .unwrap()
            .payload["streams"][0]["state"],
        "encoding"
    );
    assert_eq!(
        controller
            .handle("a", Operation::Status { stream: Some(id) })
            .unwrap()
            .payload["streams"][0]["bitrate"],
        64_000
    );
    assert_eq!(
        controller
            .handle(
                "a",
                Operation::Lookup {
                    operation_id: "failed".into()
                }
            )
            .unwrap()
            .payload["stream"],
        id
    );
    assert_eq!(controller.close("a", id).unwrap()["ok"], true);
    assert_eq!(controller.streams.len(), 0);
}

#[test]
fn recovery_preserves_accepted_packets_then_drains_new_epoch_before_release() {
    unsafe extern "C" fn collect(
        context: *mut c_void,
        packet: *const EncodedAudioPacket,
    ) -> Status {
        // SAFETY: this test keeps both the vector and packet live through the callback.
        let delivered = unsafe { &mut *context.cast::<Vec<(u64, u32, usize)>>() };
        let packet = unsafe { &*packet };
        delivered.push((packet.sequence, packet.flags, packet.len));
        STATUS_OK
    }

    let mut controller = Controller::new(true);
    let id = controller
        .handle("owner", open(Some("recovery")))
        .unwrap()
        .created
        .unwrap();
    controller
        .handle(
            "owner",
            Operation::Update {
                stream: id,
                bitrate: 64_000,
            },
        )
        .unwrap();
    let samples = vec![0u8; 480 * 2 * 2];
    let frame = AudioFrame {
        sample_rate: SAMPLE_RATE,
        channels: 2,
        format: PCM_I16,
        frames: 480,
        flags: 0,
        data: samples.as_ptr(),
        len: samples.len(),
        timestamp_us: 1000,
        sequence: 0,
    };
    // SAFETY: PCM and sink allocations remain live during the synchronous calls.
    assert_eq!(unsafe { controller.submit("owner", id, &frame) }, STATUS_OK);
    controller.streams.get_mut(&id).unwrap().failure = Some("codec_encode_failed");
    assert_eq!(controller.drain("owner", id), MEDIA_BACKEND_ERROR);
    controller
        .handle(
            "owner",
            Operation::Recover {
                stream: id,
                bitrate: None,
            },
        )
        .unwrap();
    assert_eq!(
        controller
            .handle(
                "owner",
                Operation::Lookup {
                    operation_id: "recovery".into()
                }
            )
            .unwrap()
            .payload["stream"],
        id
    );
    let mut delivered: Vec<(u64, u32, usize)> = Vec::new();
    let sink = AudioPacketSink {
        context: (&mut delivered as *mut Vec<(u64, u32, usize)>).cast(),
        write: collect,
    };
    assert_eq!(unsafe { controller.receive("owner", id, sink) }, STATUS_OK);
    assert_eq!(delivered[0].0, 0);
    assert_eq!(delivered[0].1, 0);
    assert_eq!(
        unsafe { controller.receive("owner", id, sink) },
        MEDIA_PENDING
    );
    assert_eq!(
        unsafe {
            controller.submit(
                "owner",
                id,
                &AudioFrame {
                    timestamp_us: 100_000,
                    sequence: 20,
                    ..frame
                },
            )
        },
        STATUS_OK
    );
    assert_eq!(unsafe { controller.receive("owner", id, sink) }, STATUS_OK);
    assert_eq!(delivered[1].0, 1);
    assert_eq!(delivered[1].1, AUDIO_PACKET_DISCONTINUITY);
    assert_eq!(delivered[1].2, delivered[0].2);
    assert_eq!(controller.drain("owner", id), STATUS_OK);
    while unsafe { controller.receive("owner", id, sink) } == STATUS_OK {}
    assert_eq!(delivered.last().unwrap().1, AUDIO_PACKET_END_OF_STREAM);
    assert_eq!(
        unsafe { controller.receive("owner", id, sink) },
        MEDIA_CLOSED
    );
    controller.close("owner", id).unwrap();
    assert_eq!(
        unsafe { controller.receive("owner", id, sink) },
        MEDIA_CLOSED
    );
}

#[test]
fn abandoned_eos_allows_bitrate_recovery_and_retried_delivery_before_release() {
    struct Receiver {
        packets: Vec<(u64, u32, usize)>,
        reject_next: bool,
    }
    unsafe extern "C" fn receive_packet(
        context: *mut c_void,
        packet: *const EncodedAudioPacket,
    ) -> Status {
        // SAFETY: the receiver and borrowed packet live through this call.
        let receiver = unsafe { &mut *context.cast::<Receiver>() };
        let packet = unsafe { &*packet };
        receiver
            .packets
            .push((packet.sequence, packet.flags, packet.len));
        if std::mem::take(&mut receiver.reject_next) {
            MEDIA_BUSY
        } else {
            STATUS_OK
        }
    }

    let mut controller = Controller::new(true);
    let id = controller
        .handle("owner", open(Some("abandoned-eos")))
        .unwrap()
        .created
        .unwrap();
    let original_bytes = controller.streams[&id].packet_bytes();
    assert_eq!(controller.drain("owner", id), STATUS_OK);
    assert_eq!(controller.streams[&id].queued(), 1);
    controller.streams.get_mut(&id).unwrap().failure = Some("codec_encode_failed");
    let mut receiver = Receiver {
        packets: Vec::new(),
        reject_next: true,
    };
    let sink = AudioPacketSink {
        context: (&mut receiver as *mut Receiver).cast(),
        write: receive_packet,
    };
    // SAFETY: receiver allocation lives through every synchronous callback.
    assert_eq!(
        unsafe { controller.receive("owner", id, sink) },
        MEDIA_BACKEND_ERROR
    );
    assert!(receiver.packets.is_empty());
    let recovered = controller
        .handle(
            "owner",
            Operation::Recover {
                stream: id,
                bitrate: Some(64_000),
            },
        )
        .unwrap();
    assert_eq!(recovered.payload["stream"], id);
    assert_eq!(recovered.payload["bitrate"], 64_000);
    assert_ne!(controller.streams[&id].packet_bytes(), original_bytes);
    assert_eq!(controller.streams[&id].queued(), 0);
    assert_eq!(
        unsafe { controller.receive("owner", id, sink) },
        MEDIA_PENDING
    );

    let samples = vec![0u8; 480 * 2 * 2];
    let frame = AudioFrame {
        sample_rate: SAMPLE_RATE,
        channels: 2,
        format: PCM_I16,
        frames: 480,
        flags: 0,
        data: samples.as_ptr(),
        len: samples.len(),
        timestamp_us: 25_000,
        sequence: 9,
    };
    assert_eq!(unsafe { controller.submit("owner", id, &frame) }, STATUS_OK);
    assert_eq!(unsafe { controller.receive("owner", id, sink) }, MEDIA_BUSY);
    assert_eq!(controller.streams[&id].queued(), 1);
    assert_eq!(unsafe { controller.receive("owner", id, sink) }, STATUS_OK);
    assert_eq!(receiver.packets[0], receiver.packets[1]);
    assert_eq!(receiver.packets[1].0, 0);
    assert_eq!(receiver.packets[1].1, 0);
    assert_eq!(
        receiver.packets[1].2,
        controller.streams[&id].packet_bytes()
    );
    assert_eq!(controller.drain("owner", id), STATUS_OK);
    while unsafe { controller.receive("owner", id, sink) } == STATUS_OK {}
    assert_eq!(
        receiver.packets.last().unwrap().1,
        AUDIO_PACKET_END_OF_STREAM
    );
    assert_eq!(controller.close("owner", id).unwrap()["ok"], true);
    assert_eq!(
        unsafe { controller.receive("owner", id, sink) },
        MEDIA_CLOSED
    );
}

#[test]
fn unpublished_opens_release_operation_capacity_without_erasing_close_tombstones() {
    let mut controller = Controller::new(true);
    for index in 0..1025 {
        let operation_id = format!("cancelled-{index}");
        let id = controller
            .handle("a", open(Some(&operation_id)))
            .unwrap()
            .created
            .unwrap();
        controller.rollback_unpublished_open("b", id);
        assert_eq!(
            controller
                .handle(
                    "a",
                    Operation::Lookup {
                        operation_id: operation_id.clone(),
                    },
                )
                .unwrap()
                .payload["stream"],
            id
        );
        controller.rollback_unpublished_open("a", id);
        let replacement = controller
            .handle("a", open(Some(&operation_id)))
            .unwrap()
            .created
            .unwrap();
        controller.rollback_unpublished_open("a", replacement);
        assert!(controller.issued.is_empty());
    }
    let id = controller
        .handle("a", open(Some("published")))
        .unwrap()
        .created
        .unwrap();
    controller.close("a", id).unwrap();
    controller.rollback_unpublished_open("a", id);
    assert_eq!(
        controller.handle("a", open(Some("published"))).err(),
        Some("stream_closed")
    );
}

#[test]
fn close_retry_after_failed_recovery_releases_once_and_keeps_owner_isolation() {
    let mut controller = Controller::new(true);
    let id = controller
        .handle("owner", open(Some("lease")))
        .unwrap()
        .created
        .unwrap();
    controller.streams.get_mut(&id).unwrap().failure = Some("codec_encode_failed");
    assert_eq!(
        controller
            .handle(
                "foreign",
                Operation::Recover {
                    stream: id,
                    bitrate: None,
                },
            )
            .err(),
        Some("stream_not_found")
    );
    assert_eq!(controller.close("foreign", id), Err("stream_not_found"));
    assert_eq!(controller.close("owner", id).unwrap(), json!({"ok":true}));
    assert!(!controller.streams.contains_key(&id));

    // The first close response may be lost. Repeating it must acknowledge the
    // already released lease without revealing its ID to another principal.
    assert_eq!(controller.close("owner", id).unwrap(), json!({"ok":true}));
    assert_eq!(controller.close("foreign", id), Err("stream_not_found"));
    assert_eq!(controller.drain("owner", id), MEDIA_CLOSED);
    assert_eq!(
        controller
            .handle(
                "owner",
                Operation::Recover {
                    stream: id,
                    bitrate: None,
                },
            )
            .err(),
        Some("stream_not_found")
    );
    assert_eq!(
        controller.handle("owner", open(Some("lease"))).err(),
        Some("stream_closed")
    );
    let replacement = controller
        .handle("owner", open(None))
        .unwrap()
        .created
        .unwrap();
    assert_ne!(replacement, id);
    assert_eq!(
        controller.close("owner", replacement).unwrap(),
        json!({"ok":true})
    );
}
