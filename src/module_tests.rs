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
