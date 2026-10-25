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
use serde_json::json;
use std::sync::{Arc, Mutex};

const SERVICE: &str = "amaterasu.audio-encode/1";

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    enabled: bool,
}

struct AudioEncodeModule {
    runtime: RuntimeClient,
    controller: native::Shared,
    _factory: Box<native::Factory>,
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
            runtime.register_native(SERVICE, &factory.api)?;
        }
        Ok(Self {
            runtime,
            controller,
            _factory: factory,
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        match self.runtime.send(Command::Poll {})? {
            Response::Pending => Ok(()),
            Response::Request {
                id,
                caller,
                service,
                payload,
                ..
            } => {
                if !matches!(
                    self.runtime.send(Command::CheckRequest { request: id })?,
                    Response::Ok
                ) {
                    return Ok(());
                }
                let result = if service == SERVICE {
                    serde_json::from_value::<Operation>(payload)
                        .map_err(|_| "invalid_request")
                        .and_then(|operation| {
                            self.controller
                                .lock()
                                .map_err(|_| "encoder_state_failed")?
                                .handle(&caller, operation)
                        })
                } else {
                    Err("invalid_service")
                };
                let ControlReply { payload, created } =
                    result.unwrap_or_else(|code| ControlReply {
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
                    let _ = self.controller.lock().map(|mut controller| {
                        controller.rollback_unpublished_open(&caller, stream)
                    });
                }
                Ok(())
            }
            _ => Err("audio encoder poll failed".into()),
        }
    }
}

impl Drop for AudioEncodeModule {
    fn drop(&mut self) {
        // A panic in a prior control operation can poison this mutex. Native
        // bindings still hold the controller Arc, so unload must clear codec
        // state even when the lock reports poison.
        self.controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
    }
}

amaterasu_module_sdk::export_module!(AudioEncodeModule);

#[cfg(test)]
mod module_tests;
