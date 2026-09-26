use super::*;

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
                channels: 6,
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
                bitrate: 64_000,
            },
        )
        .unwrap();
    assert_eq!(reply.payload["bitrate"], 64_000);
    assert!(reply.payload["packet_bytes"].as_u64().unwrap() > 0);
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
                    operation_id: "rate".into()
                }
            )
            .unwrap()
            .payload["bitrate"],
        64_000
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
            .handle("b", Operation::Recover { stream: id })
            .err(),
        Some("stream_not_found")
    );
    controller.streams.get_mut(&id).unwrap().failure = Some("codec_encode_failed");
    assert_eq!(
        controller
            .handle("b", Operation::Recover { stream: id })
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
    let reply = controller
        .handle("a", Operation::Recover { stream: id })
        .unwrap();
    assert_eq!(reply.payload["stream"], id);
    assert_eq!(
        controller
            .handle("a", Operation::Status { stream: Some(id) })
            .unwrap()
            .payload["streams"][0]["state"],
        "encoding"
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
