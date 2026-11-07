//! Bounded caller-owned libopus encoding through the borrowed native media ABI.
mod codec;
mod controller;
mod model;
mod native;
mod stream;

use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};
use controller::{ControlReply, Controller};
use model::Operation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, TryLockError};

const SERVICE: &str = "amaterasu.audio-encode/1";

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    enabled: bool,
}

struct AudioEncodeModule {
    runtime: RuntimeClient,
    controller: native::Shared,
    closing: bool,
    pending: Option<ControlRequest>,
    rollback: Option<(String, u64)>,
    _factory: Box<native::Factory>,
}

struct ControlRequest {
    id: u64,
    caller: String,
    service: String,
    payload: Value,
}

impl ManagedModule for AudioEncodeModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("audio encoder settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|_| "invalid audio encoder settings")?;
        let controller = Arc::new(Mutex::new(Controller::new(settings.enabled)));
        let factory = native::Factory::new(controller.clone());
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: boxed factory and synchronized controller live until unload.
        // Signed dependency ordering requires consumers to release all native
        // bindings before provider unload; grants are not a native-code sandbox.
        unsafe {
            amaterasu_module_sdk::audio::register_encoder(&runtime, SERVICE, &factory.api)?;
        }
        Ok(Self {
            runtime,
            controller,
            closing: false,
            pending: None,
            rollback: None,
            _factory: factory,
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        let _priority = amaterasu_module_sdk::threading::ScopedThreadPriority::best_effort(
            amaterasu_module_sdk::threading::ThreadPriority::High,
        );
        if !self.finish_rollback() {
            return Ok(());
        }
        let request = if let Some(request) = self.pending.take() {
            request
        } else {
            match self.runtime.send(Command::Poll {})? {
                Response::Pending => return Ok(()),
                Response::Request {
                    id,
                    caller,
                    service,
                    payload,
                    ..
                } => ControlRequest {
                    id,
                    caller,
                    service,
                    payload,
                },
                _ => return Err("audio encoder poll failed".into()),
            }
        };
        self.handle_request(request)
    }

    fn prepare_shutdown(&mut self, _remaining_ms: u64) -> ModuleResult<()> {
        // Announcement precedes consumer shutdown; keep their native stream
        // calls and owner cleanup available until the host begins quiescence.
        self.closing = true;
        Ok(())
    }

    fn quiesce(&mut self) -> ModuleResult<bool> {
        self.closing = true;
        // A broker request already taken by this module still needs a terminal
        // response; keep ticking it rather than dropping its cleanup ownership.
        if self.pending.is_some() || !self.finish_rollback() {
            return Ok(false);
        }
        match self.controller.try_lock() {
            Ok(mut controller) => controller.shutdown(),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().shutdown(),
            Err(TryLockError::WouldBlock) => return Ok(false),
        }
        Ok(true)
    }
}

impl AudioEncodeModule {
    fn handle_request(&mut self, request: ControlRequest) -> ModuleResult<()> {
        let checked = match self.runtime.send(Command::CheckRequest {
            request: request.id,
        }) {
            Ok(response) => response,
            Err(error) => {
                self.pending = Some(request);
                return Err(error);
            }
        };
        if !matches!(checked, Response::Ok) {
            return Ok(());
        }
        let ControlRequest {
            id,
            caller,
            service,
            payload,
        } = request;
        let operation = if service == SERVICE {
            serde_json::from_value::<Operation>(payload.clone()).map_err(|_| "invalid_request")
        } else {
            Err("invalid_service")
        };
        let result =
            match operation {
                Ok(
                    Operation::Open { .. } | Operation::Update { .. } | Operation::Recover { .. },
                ) if self.closing => Err("shutting_down"),
                Ok(operation) => match self.controller.try_lock() {
                    Ok(mut controller) => controller.handle(&caller, operation),
                    Err(TryLockError::WouldBlock) => {
                        self.pending = Some(ControlRequest {
                            id,
                            caller,
                            service,
                            payload,
                        });
                        return Ok(());
                    }
                    Err(TryLockError::Poisoned(_)) => Err("encoder_state_failed"),
                },
                Err(code) => Err(code),
            };
        let ControlReply { payload, created } = result.unwrap_or_else(|code| ControlReply {
            payload: json!({"ok":false,"code":code}),
            created: None,
        });
        if !matches!(
            self.runtime.send(Command::Complete {
                request: id,
                payload
            }),
            Ok(Response::Ok)
        ) && let Some(stream) = created
        {
            // A lost/cancelled open response cannot leave a hidden codec.
            self.rollback = Some((caller, stream));
            self.finish_rollback();
        }
        Ok(())
    }

    fn finish_rollback(&mut self) -> bool {
        let Some((owner, stream)) = self.rollback.as_ref() else {
            return true;
        };
        match self.controller.try_lock() {
            Ok(mut controller) => controller.rollback_unpublished_open(owner, *stream),
            Err(TryLockError::Poisoned(poisoned)) => poisoned
                .into_inner()
                .rollback_unpublished_open(owner, *stream),
            Err(TryLockError::WouldBlock) => return false,
        }
        self.rollback = None;
        true
    }
}

impl Drop for AudioEncodeModule {
    fn drop(&mut self) {
        // A panic in a prior control operation can poison this mutex. Native
        // bindings still hold the controller Arc, so unload must clear codec
        // state even when the lock reports poison.
        // Preserve the native ABI's join barrier: an Arc pins controller data,
        // not this library's code. Returning while a sink callback still executes
        // would allow unsafe code unload. Declared consumers must end all calls
        // and release their bindings before provider destruction.
        self.controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
    }
}

amaterasu_module_sdk::export_module!(AudioEncodeModule);

#[cfg(test)]
mod module_tests;
