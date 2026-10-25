use super::*;
use crate::model::Operation;
use amaterasu_media::{MEDIA_CLOSED, MEDIA_PENDING, PCM_I16};

unsafe fn bind_owner(factory: &Factory, owner: &str) -> *const AudioEncoderApi {
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: INTERFACE_MAJOR,
        min_minor: INTERFACE_MINOR,
        min_size: size_of::<AudioEncoderApi>() as u32,
    };
    let mut output = std::ptr::null();
    // SAFETY: fixture factory, owner/query/output all live through binding.
    assert_eq!(
        unsafe {
            (factory.api.bind.unwrap())(
                factory.api.context,
                BorrowedBytes::new(owner.as_bytes()),
                &query,
                &mut output,
            )
        },
        STATUS_OK
    );
    output.cast()
}

#[test]
fn bound_tables_enforce_owner_reentrancy_close_and_shutdown_lifetimes() {
    let shared = Arc::new(Mutex::new(Controller::new(true)));
    let factory = Factory::new(shared.clone());
    let id = shared
        .lock()
        .unwrap()
        .handle(
            "a",
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
    // SAFETY: bindings are released exactly once after all test callbacks finish.
    unsafe {
        let owner = bind_owner(&factory, "a");
        let foreign = bind_owner(&factory, "b");
        let a = &*owner;
        let b = &*foreign;
        let bytes = [0u8; 480 * 2];
        let frame = AudioFrame {
            sample_rate: 48_000,
            channels: 1,
            format: PCM_I16,
            frames: 480,
            flags: 0,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 0,
            sequence: 0,
        };
        struct Receiver {
            calls: usize,
            table: *const AudioEncoderApi,
            id: u64,
        }
        unsafe extern "C" fn reject(
            context: *mut c_void,
            _: *const amaterasu_media::EncodedAudioPacket,
        ) -> Status {
            // SAFETY: test keeps receiver and table alive; reentrant call must fail
            // fast rather than deadlock or mutate the currently delivered packet.
            let receiver = unsafe { &mut *context.cast::<Receiver>() };
            receiver.calls += 1;
            let api = unsafe { &*receiver.table };
            assert_eq!(
                unsafe { (api.drain)(api.base.context, receiver.id) },
                MEDIA_BUSY
            );
            MEDIA_BUSY
        }
        let mut receiver = Receiver {
            calls: 0,
            table: owner,
            id,
        };
        let sink = AudioPacketSink {
            context: (&mut receiver as *mut Receiver).cast(),
            write: reject,
        };
        assert_eq!((b.submit)(b.base.context, id, &frame), MEDIA_CLOSED);
        assert_eq!((b.receive)(b.base.context, id, sink), MEDIA_CLOSED);
        assert_eq!((b.drain)(b.base.context, id), MEDIA_CLOSED);
        assert_eq!(receiver.calls, 0);
        assert_eq!((a.receive)(a.base.context, id, sink), MEDIA_PENDING);
        assert_eq!((a.submit)(a.base.context, id, &frame), STATUS_OK);
        assert_eq!((a.receive)(a.base.context, id, sink), MEDIA_BUSY);
        assert_eq!(receiver.calls, 1);
        shared.lock().unwrap().close("a", id).unwrap();
        assert_eq!((a.receive)(a.base.context, id, sink), MEDIA_CLOSED);
        shared.lock().unwrap().shutdown();
        drop(factory);
        assert_eq!((a.drain)(a.base.context, id), MEDIA_CLOSED);
        (a.base.release.unwrap())(a.base.context);
        (b.base.release.unwrap())(b.base.context);
    }
}

#[test]
fn poisoned_provider_binding_closes_after_shutdown() {
    let shared = Arc::new(Mutex::new(Controller::new(true)));
    let factory = Factory::new(shared.clone());
    let id = shared
        .lock()
        .unwrap()
        .handle(
            "owner",
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
    // SAFETY: the fixture keeps this binding alive through each call and
    // releases it once all calls have returned.
    unsafe {
        let table = bind_owner(&factory, "owner");
        let api = &*table;
        let bytes = [0u8; 480 * 2];
        let frame = AudioFrame {
            sample_rate: 48_000,
            channels: 1,
            format: PCM_I16,
            frames: 480,
            flags: 0,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 0,
            sequence: 0,
        };
        shared
            .lock()
            .unwrap()
            .handle(
                "owner",
                Operation::Update {
                    stream: id,
                    bitrate: 96_000,
                },
            )
            .unwrap();
        assert_eq!((api.submit)(api.base.context, id, &frame), STATUS_OK);
        unsafe extern "C" fn accept(
            context: *mut c_void,
            packet: *const amaterasu_media::EncodedAudioPacket,
        ) -> Status {
            // SAFETY: the counter and packet remain live for this callback.
            let delivered = unsafe { &mut *context.cast::<usize>() };
            assert!(!packet.is_null());
            *delivered += 1;
            STATUS_OK
        }
        let mut delivered = 0usize;
        let sink = AudioPacketSink {
            context: (&mut delivered as *mut usize).cast(),
            write: accept,
        };
        assert_eq!((api.receive)(api.base.context, id, sink), STATUS_OK);
        assert_eq!(delivered, 1);
        assert!(
            std::thread::spawn({
                let shared = shared.clone();
                move || {
                    let _guard = shared.lock().unwrap();
                    panic!("poison live encoder controller");
                }
            })
            .join()
            .is_err()
        );
        // Recovery requires the poisoned controller lock and is intentionally
        // unavailable; module teardown still clears its owned codec state.
        assert_eq!((api.drain)(api.base.context, id), MEDIA_BACKEND_ERROR);
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
        drop(factory);
        assert_eq!((api.submit)(api.base.context, id, &frame), MEDIA_CLOSED);
        assert_eq!((api.drain)(api.base.context, id), MEDIA_CLOSED);
        (api.base.release.unwrap())(api.base.context);
    }
}
