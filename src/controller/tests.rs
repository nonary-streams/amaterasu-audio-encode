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
