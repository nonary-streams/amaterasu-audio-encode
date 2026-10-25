use crate::controller::Controller;
use amaterasu_media::{
    AudioEncoderApi, AudioFrame, AudioPacketSink, INTERFACE_MAJOR, INTERFACE_MINOR,
    MEDIA_BACKEND_ERROR, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_UNSUPPORTED,
};
use amaterasu_module_sdk::abi::*;
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, TryLockError},
};

pub type Shared = Arc<Mutex<Controller>>;

pub struct Factory {
    pub api: NativeServiceFactory,
    shared: Shared,
}

impl Factory {
    pub fn new(shared: Shared) -> Box<Self> {
        let mut factory = Box::new(Self {
            api: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context: std::ptr::null_mut(),
                bind: Some(bind),
            },
            shared,
        });
        factory.api.context = (&mut *factory as *mut Self).cast();
        factory
    }
}

struct Binding {
    api: AudioEncoderApi,
    shared: Shared,
    owner: String,
}

unsafe extern "C" fn bind(
    context: *mut c_void,
    principal: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guarded(|| {
        if context.is_null() || query.is_null() || output.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the runtime supplies a live boxed factory, query, output and
        // callback-lifetime principal. Only the copied owner and Arc are retained.
        unsafe {
            *output = std::ptr::null();
            let query = &*query;
            if !query.header.supports::<NativeInterfaceQuery>()
                || query.interface_major != INTERFACE_MAJOR
                || query.min_minor > INTERFACE_MINOR
                || query.min_size as usize > size_of::<AudioEncoderApi>()
            {
                return MEDIA_UNSUPPORTED;
            }
            let owner = match principal
                .read()
                .ok()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
            {
                Some(owner) if !owner.is_empty() && owner.len() <= 128 => owner.to_owned(),
                _ => return STATUS_INVALID_ARGUMENT,
            };
            let mut binding = Box::new(Binding {
                api: AudioEncoderApi {
                    base: NativeInterface {
                        header: AbiHeader {
                            size: size_of::<AudioEncoderApi>() as u32,
                            major: INTERFACE_MAJOR,
                            minor: INTERFACE_MINOR,
                        },
                        context: std::ptr::null_mut(),
                        release: Some(release),
                    },
                    submit,
                    receive,
                    drain,
                },
                shared: (&*context.cast::<Factory>()).shared.clone(),
                owner,
            });
            binding.api.base.context = (&mut *binding as *mut Binding).cast();
            let pointer = Box::into_raw(binding);
            *output = &(*pointer).api.base;
        }
        STATUS_OK
    })
}

unsafe extern "C" fn release(context: *mut c_void) {
    let _ = guarded(|| {
        if !context.is_null() {
            // SAFETY: this unique binding is released once, after all calls end.
            drop(unsafe { Box::from_raw(context.cast::<Binding>()) });
        }
        STATUS_OK
    });
}

unsafe extern "C" fn submit(context: *mut c_void, id: u64, frame: *const AudioFrame) -> Status {
    guarded(|| {
        if frame.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: native caller supplies the table's live context and a readable
        // AudioFrame plus initialized PCM, all borrowed until this call returns.
        unsafe {
            with_controller(context, |controller, owner| {
                controller.submit(owner, id, &*frame)
            })
        }
    })
}

unsafe extern "C" fn receive(context: *mut c_void, id: u64, sink: AudioPacketSink) -> Status {
    guarded(|| {
        // SAFETY: caller keeps binding and sink alive for synchronous delivery.
        unsafe {
            with_controller(context, |controller, owner| {
                controller.receive(owner, id, sink)
            })
        }
    })
}

unsafe extern "C" fn drain(context: *mut c_void, id: u64) -> Status {
    guarded(|| {
        // SAFETY: binding remains live throughout the call.
        unsafe { with_controller(context, |controller, owner| controller.drain(owner, id)) }
    })
}

unsafe fn with_controller(
    context: *mut c_void,
    operation: impl FnOnce(&mut Controller, &str) -> Status,
) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the callback caller guarantees this live binding allocation.
    let binding = unsafe { &*context.cast::<Binding>() };
    let mut controller = match binding.shared.try_lock() {
        Ok(controller) => controller,
        Err(TryLockError::WouldBlock) => return MEDIA_BUSY,
        Err(TryLockError::Poisoned(poisoned)) => {
            // Module drop can still shut down a poisoned controller. A retained
            // binding then observes a closed provider, not a live codec fault.
            return if poisoned.into_inner().is_shutdown() {
                MEDIA_CLOSED
            } else {
                MEDIA_BACKEND_ERROR
            };
        }
    };
    operation(&mut controller, &binding.owner)
}

fn guarded(operation: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(STATUS_INTERNAL_ERROR)
}

#[cfg(test)]
mod tests;
