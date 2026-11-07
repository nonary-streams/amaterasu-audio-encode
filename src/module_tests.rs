use super::*;
use amaterasu_module_sdk::abi::*;
use std::{collections::VecDeque, ffi::c_void};

struct Host {
    replies: VecDeque<Response>,
    complete: Option<serde_json::Value>,
}

unsafe extern "C" fn invoke(
    context: *mut c_void,
    input: BorrowedBytes,
    output: ReplySink,
) -> Status {
    // SAFETY: the test endpoint state and byte buffers live for this invocation.
    let host = unsafe { &mut *context.cast::<Host>() };
    let command: Command = serde_json::from_slice(unsafe { input.read().unwrap() }).unwrap();
    if let Command::Complete { payload, .. } = command {
        host.complete = Some(payload);
    }
    let response = host.replies.pop_front().expect("unexpected runtime call");
    unsafe { output.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

fn request() -> Response {
    serde_json::from_value(json!({"kind":"request","remaining_ms":1000,"id":1,"caller":"consumer","initiator":"consumer","service":SERVICE,"payload":{"action":"open","operation_id":"cancelled"}})).unwrap()
}

#[test]
fn cancellation_before_dispatch_and_after_open_cannot_leave_hidden_sessions() {
    for cancelled_before in [true, false] {
        let ended = Response::Error {
            code: "request_ended".into(),
            message: "ended".into(),
        };
        let mut host = Host {
            replies: VecDeque::from([
                Response::Result {
                    payload: json!({"enabled":true}),
                },
                Response::Ok,
                Response::Ok,
                request(),
                if cancelled_before {
                    ended.clone()
                } else {
                    Response::Ok
                },
            ]),
            complete: None,
        };
        if !cancelled_before {
            host.replies.push_back(ended);
        }
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&mut host as *mut Host).cast(),
            invoke,
        };
        // SAFETY: host state outlives every runtime call and module destruction.
        let mut module =
            AudioEncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
        module.tick().unwrap();
        assert!(host.replies.is_empty());
        assert_eq!(host.complete.is_some(), !cancelled_before);
        let status = module
            .controller
            .lock()
            .unwrap()
            .handle("consumer", Operation::Status { stream: None })
            .unwrap()
            .payload;
        assert_eq!(status["streams"], json!([]));
        let retry = module
            .controller
            .lock()
            .unwrap()
            .handle(
                "consumer",
                Operation::Open {
                    channels: 2,
                    packet_ms: 10,
                    bitrate: 128_000,
                    operation_id: Some("cancelled".into()),
                },
            )
            .unwrap();
        assert!(retry.created.is_some());
    }
}

#[test]
fn module_drop_releases_codec_sessions_after_controller_poison() {
    for poison_lock in [false, true] {
        let mut host = Host {
            replies: VecDeque::from([
                Response::Result {
                    payload: json!({"enabled":true}),
                },
                Response::Ok,
                Response::Ok,
            ]),
            complete: None,
        };
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&mut host as *mut Host).cast(),
            invoke,
        };
        // SAFETY: host state remains live until after module destruction.
        let module =
            AudioEncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
        let shared = module.controller.clone();
        let stream = shared
            .lock()
            .unwrap()
            .handle(
                "consumer",
                Operation::Open {
                    channels: 2,
                    packet_ms: 10,
                    bitrate: 128_000,
                    operation_id: None,
                },
            )
            .unwrap()
            .created
            .unwrap();
        if poison_lock {
            let poisoned = shared.clone();
            assert!(
                std::thread::spawn(move || {
                    let _guard = poisoned.lock().unwrap();
                    panic!("poison controller after opening codec");
                })
                .join()
                .is_err()
            );
        }

        drop(module);
        assert_eq!(shared.is_poisoned(), poison_lock);
        let mut controller = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = controller
            .handle("consumer", Operation::Status { stream: None })
            .unwrap()
            .payload;
        assert_eq!(status["enabled"], false);
        assert_eq!(status["streams"], json!([]));
        assert_eq!(
            controller.close("consumer", stream),
            Err("stream_not_found")
        );
    }
}

#[test]
fn quiesce_does_not_report_codec_retirement_while_a_native_call_owns_state() {
    let mut host = Host {
        replies: VecDeque::from([
            Response::Result {
                payload: json!({"enabled":true}),
            },
            Response::Ok,
            Response::Ok,
        ]),
        complete: None,
    };
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&mut host as *mut Host).cast(),
        invoke,
    };
    // SAFETY: the fixture endpoint outlives module shutdown and destruction.
    let mut module =
        AudioEncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
    let shared = module.controller.clone();
    let mut controller = shared.lock().unwrap();
    controller
        .handle(
            "consumer",
            Operation::Open {
                channels: 2,
                packet_ms: 10,
                bitrate: 128_000,
                operation_id: None,
            },
        )
        .unwrap();

    // Native receive owns this same lock while its packet callback executes.
    assert!(!module.quiesce().unwrap());
    assert!(!controller.is_shutdown());
    drop(controller);

    assert!(module.quiesce().unwrap());
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .handle("consumer", Operation::Status { stream: None })
            .unwrap()
            .payload["streams"],
        json!([])
    );
}

#[test]
fn busy_close_is_retained_without_blocking_and_cancellation_is_rechecked() {
    for cancelled in [false, true] {
        let mut host = Host {
            replies: VecDeque::from([
                Response::Result {
                    payload: json!({"enabled":true}),
                },
                Response::Ok,
                Response::Ok,
            ]),
            complete: None,
        };
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&mut host as *mut Host).cast(),
            invoke,
        };
        // SAFETY: the fixture host remains live until the module is destroyed.
        let mut module =
            AudioEncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
        let stream = module
            .controller
            .lock()
            .unwrap()
            .handle(
                "consumer",
                Operation::Open {
                    channels: 2,
                    packet_ms: 10,
                    bitrate: 128_000,
                    operation_id: None,
                },
            )
            .unwrap()
            .created
            .unwrap();
        host.replies.extend([
            serde_json::from_value(json!({"kind":"request","remaining_ms":1000,"id":2,"caller":"consumer","initiator":"consumer","service":SERVICE,"payload":{"action":"close","stream":stream}})).unwrap(),
            Response::Ok,
        ]);

        let (ready, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let shared = module.controller.clone();
        let owner = std::thread::spawn(move || {
            let _native_call = shared.lock().unwrap();
            ready.send(()).unwrap();
            // Bound the red-case failure too: a blocking tick must not hang the
            // test process. Success requires the caller to release this owner.
            released
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
        });
        started.recv().unwrap();
        module.tick().unwrap();
        let retained = module.pending.is_some();
        let completed = host.complete.is_some();
        let _ = release.send(());
        let released_by_caller = owner.join().unwrap();
        assert!(
            released_by_caller,
            "tick waited on a native packet callback"
        );
        assert!(retained);
        assert!(!completed);

        if cancelled {
            host.replies.push_back(Response::Error {
                code: "request_ended".into(),
                message: "cancelled".into(),
            });
        } else {
            host.replies.extend([Response::Ok, Response::Ok]);
        }
        module.tick().unwrap();
        assert!(module.pending.is_none());
        assert_eq!(host.complete.is_some(), !cancelled);
        let status = module
            .controller
            .lock()
            .unwrap()
            .handle("consumer", Operation::Status { stream: None })
            .unwrap()
            .payload;
        assert_eq!(
            status["streams"].as_array().unwrap().len(),
            usize::from(cancelled)
        );
        assert!(module.quiesce().unwrap());
    }
}

#[test]
fn shutdown_announcement_closes_admission_without_retiring_consumer_codec() {
    let mut host = Host {
        replies: VecDeque::from([
            Response::Result {
                payload: json!({"enabled":true}),
            },
            Response::Ok,
            Response::Ok,
        ]),
        complete: None,
    };
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&mut host as *mut Host).cast(),
        invoke,
    };
    // SAFETY: the host and factory contexts outlive every fixture call.
    let mut module =
        AudioEncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
    let stream = module
        .controller
        .lock()
        .unwrap()
        .handle(
            "consumer",
            Operation::Open {
                channels: 2,
                packet_ms: 10,
                bitrate: 128_000,
                operation_id: None,
            },
        )
        .unwrap()
        .created
        .unwrap();
    module.prepare_shutdown(0).unwrap();
    assert_eq!(
        module.controller.lock().unwrap().drain("consumer", stream),
        STATUS_OK
    );
    host.replies.extend([request(), Response::Ok, Response::Ok]);
    module.tick().unwrap();
    assert_eq!(
        host.complete.take().unwrap(),
        json!({"ok":false,"code":"shutting_down"})
    );
    assert!(!module.controller.lock().unwrap().is_shutdown());
    host.replies.extend([
        serde_json::from_value(json!({"kind":"request","remaining_ms":1000,"id":3,"caller":"consumer","initiator":"consumer","service":SERVICE,"payload":{"action":"close","stream":stream}})).unwrap(),
        Response::Ok,
        Response::Ok,
    ]);
    module.tick().unwrap();
    assert_eq!(host.complete.take().unwrap(), json!({"ok":true}));
    assert!(module.quiesce().unwrap());
}

#[test]
fn native_packet_borrow_remains_live_until_callback_returns_before_quiescence() {
    use amaterasu_media::{
        AudioEncoderApi, AudioFrame, AudioPacketSink, EncodedAudioPacket, INTERFACE_MAJOR,
        INTERFACE_MINOR, PCM_I16,
    };
    let mut host = Host {
        replies: VecDeque::from([
            Response::Result {
                payload: json!({"enabled":true}),
            },
            Response::Ok,
            Response::Ok,
        ]),
        complete: None,
    };
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&mut host as *mut Host).cast(),
        invoke,
    };
    // SAFETY: the fixture host stays alive through module destruction.
    let mut module =
        AudioEncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
    let stream = module
        .controller
        .lock()
        .unwrap()
        .handle(
            "consumer",
            Operation::Open {
                channels: 1,
                packet_ms: 10,
                bitrate: 64_000,
                operation_id: None,
            },
        )
        .unwrap()
        .created
        .unwrap();
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: INTERFACE_MAJOR,
        min_minor: INTERFACE_MINOR,
        min_size: size_of::<AudioEncoderApi>() as u32,
    };
    let mut binding = std::ptr::null();
    // SAFETY: the boxed factory, owner bytes, query and output remain valid;
    // the worker releases the unique binding after every native call ends.
    assert_eq!(
        unsafe {
            (module._factory.api.bind.unwrap())(
                module._factory.api.context,
                BorrowedBytes::new(b"consumer"),
                &query,
                &mut binding,
            )
        },
        STATUS_OK
    );
    let binding_address = binding as usize;
    let (ready, started) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    struct Receiver {
        ready: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        released_by_caller: bool,
        borrow_unchanged: bool,
    }
    unsafe extern "C" fn receive_packet(
        context: *mut c_void,
        packet: *const EncodedAudioPacket,
    ) -> Status {
        // SAFETY: the worker owns the receiver for this entire callback; packet
        // bytes are borrowed only before the callback returns.
        let receiver = unsafe { &mut *context.cast::<Receiver>() };
        let packet = unsafe { &*packet };
        let bytes = unsafe { std::slice::from_raw_parts(packet.data, packet.len) };
        let original = bytes.to_vec();
        if receiver.ready.send(()).is_err() {
            return STATUS_INTERNAL_ERROR;
        }
        receiver.released_by_caller = receiver
            .release
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok();
        receiver.borrow_unchanged = !bytes.is_empty() && bytes == original;
        STATUS_OK
    }
    let consumer = std::thread::spawn(move || {
        // SAFETY: the provider stays loaded until this worker joins, and its
        // unique binding is released once after submit and receive return.
        unsafe {
            let api = &*(binding_address as *const AudioEncoderApi);
            let pcm = [0u8; 480 * 2];
            let frame = AudioFrame {
                sample_rate: 48_000,
                channels: 1,
                format: PCM_I16,
                frames: 480,
                flags: 0,
                data: pcm.as_ptr(),
                len: pcm.len(),
                timestamp_us: 0,
                sequence: 0,
            };
            let submitted = (api.submit)(api.base.context, stream, &frame);
            let mut receiver = Receiver {
                ready,
                release: released,
                released_by_caller: false,
                borrow_unchanged: false,
            };
            let received = (api.receive)(
                api.base.context,
                stream,
                AudioPacketSink {
                    context: (&mut receiver as *mut Receiver).cast(),
                    write: receive_packet,
                },
            );
            (api.base.release.unwrap())(api.base.context);
            (
                submitted,
                received,
                receiver.released_by_caller,
                receiver.borrow_unchanged,
            )
        }
    });
    started.recv().unwrap();
    let ready_while_borrowed = module.quiesce().unwrap();
    release.send(()).unwrap();
    let (submitted, received, released_by_caller, borrow_unchanged) = consumer.join().unwrap();
    assert!(!ready_while_borrowed);
    assert!(released_by_caller);
    assert!(borrow_unchanged);
    assert_eq!(submitted, STATUS_OK);
    assert_eq!(received, STATUS_OK);
    assert!(module.quiesce().unwrap());
    assert!(module.controller.lock().unwrap().is_shutdown());
}
