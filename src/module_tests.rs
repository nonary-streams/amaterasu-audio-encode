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
    }
}
