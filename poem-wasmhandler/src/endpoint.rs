use poem::{Body, Endpoint, Request, Response, Result};
use poem_wasm::ffi::{RESPONSE_BODY_BYTES, RESPONSE_BODY_EMPTY, RESPONSE_BODY_STREAM};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use wasmtime::{Config, Engine, IntoFunc, Linker, Module, Store};

use crate::{funcs, state::WasmEndpointState, WasmHandlerError};

pub struct WasmEndpointBuilder<State>
where
    State: Send + Sync + Clone + 'static,
{
    engine: Engine,
    linker: Linker<WasmEndpointState<State>>,
    module: Vec<u8>,
    user_state: State,
}

impl WasmEndpointBuilder<()> {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        let engine = Engine::new(&Config::new().async_support(true)).unwrap();
        let linker = Linker::new(&engine);

        Self {
            engine,
            linker,
            module: bytes.into(),
            user_state: (),
        }
    }
}

impl<State> WasmEndpointBuilder<State>
where
    State: Send + Sync + Clone + 'static,
{
    pub fn with_state(self, user_state: State) -> WasmEndpointBuilder<State> {
        Self {
            user_state,
            linker: Linker::new(&self.engine),
            ..self
        }
    }

    pub fn udf<Params, Args>(
        mut self,
        module: &str,
        name: &str,
        func: impl IntoFunc<WasmEndpointState<State>, Params, Args>,
    ) -> Self {
        self.linker.func_wrap(module, name, func).unwrap();
        self
    }

    pub fn build(mut self) -> Result<WasmEndpoint<State>> {
        let module = Module::new(&self.engine, self.module)?;
        funcs::add_to_linker(&mut self.linker).unwrap();
        wasmtime_wasi::add_to_linker(&mut self.linker, |state| &mut state.wasi)?;

        Ok(WasmEndpoint {
            engine: self.engine,
            module,
            linker: self.linker,
            user_state: self.user_state,
        })
    }
}

pub struct WasmEndpoint<State> {
    engine: Engine,
    module: Module,
    linker: Linker<WasmEndpointState<State>>,
    user_state: State,
}

#[poem::async_trait]
impl<State> Endpoint for WasmEndpoint<State>
where
    State: Send + Sync + Clone + 'static,
{
    type Output = Response;

    async fn call(&self, req: Request) -> Result<Self::Output> {
        let on_upgrade = req.take_upgrade().ok();

        // create wasm instance
        let (mut response_receiver, mut response_body_receiver, upgraded_stub) = {
            let user_state = self.user_state.clone();
            let (response_sender, response_receiver) = mpsc::unbounded_channel();
            let (response_body_sender, response_body_receiver) = mpsc::unbounded_channel();
            let (upgraded, upgraded_stub) = if on_upgrade.is_some() {
                let (upgraded_reader, upgraded_writer) = tokio::io::duplex(4096);
                let (upgraded_sender, upgraded_receiver) = mpsc::unbounded_channel();
                (
                    Some((upgraded_reader, upgraded_sender)),
                    Some((upgraded_writer, upgraded_receiver)),
                )
            } else {
                (None, None)
            };
            let state = WasmEndpointState::new(
                req,
                response_sender,
                response_body_sender,
                upgraded,
                user_state,
            );
            let mut store = Store::new(&self.engine, state);
            let linker = self.linker.clone();
            let module = self.module.clone();

            // invoke main
            tokio::spawn(async move {
                let instance = match linker.instantiate_async(&mut store, &module).await {
                    Ok(instance) => instance,
                    Err(err) => {
                        tracing::error!(error = %err, "wasm instantiate error");
                        return;
                    }
                };
                let start_func = match instance.get_typed_func::<(), (), _>(&mut store, "start") {
                    Ok(start_func) => start_func,
                    Err(err) => {
                        tracing::error!(error = %err, "wasm error");
                        return;
                    }
                };
                if let Err(err) = start_func.call_async(&mut store, ()).await {
                    tracing::error!(error = %err, "wasm error");
                    return;
                }
            });

            (response_receiver, response_body_receiver, upgraded_stub)
        };

        let mut resp = Response::default();

        // receive response
        match response_receiver.recv().await {
            Some((status, headers, body_type)) => {
                resp.set_status(status);
                *resp.headers_mut() = headers;

                match body_type {
                    RESPONSE_BODY_EMPTY => resp.set_body(Body::empty()),
                    RESPONSE_BODY_BYTES => {
                        if let Some(data) = response_body_receiver.recv().await {
                            resp.set_body(data);
                        } else {
                            resp.set_body(());
                        }
                    }
                    RESPONSE_BODY_STREAM => {
                        resp.set_body(Body::from_bytes_stream(
                            tokio_stream::wrappers::UnboundedReceiverStream::new(
                                response_body_receiver,
                            )
                            .map(Ok::<_, std::io::Error>),
                        ));
                    }
                    _ => unreachable!(),
                }
            }
            None => return Err(WasmHandlerError::IncompleteResponse.into()),
        }

        // upgraded
        if let (Some(on_upgrade), Some((mut upgraded_writer, mut upgraded_receiver))) =
            (on_upgrade, upgraded_stub)
        {
            tokio::spawn(async move {
                if let Ok(upgraded) = on_upgrade.await {
                    let (mut reader, mut writer) = tokio::io::split(upgraded);
                    tokio::spawn(async move {
                        while let Some(data) = upgraded_receiver.recv().await {
                            if writer.write(&data).await.is_err() {
                                break;
                            }
                        }
                    });
                    let _ = tokio::io::copy(&mut reader, &mut upgraded_writer).await;
                }
            });
        }

        Ok(resp)
    }
}
