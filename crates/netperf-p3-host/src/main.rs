// Minimal host that wires WASI 0.3 (incl. sockets) into the linker — the piece
// the `wasmtime` CLI omits for generic command components.
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

struct MyState {
    ctx: WasiCtx,
    table: ResourceTable,
}
impl WasiView for MyState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;

    let mut linker = Linker::<MyState>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let component = Component::from_file(&engine, &args[0])?;

    let mut builder = WasiCtx::builder();
    builder.inherit_stdio().inherit_env().inherit_network().args(&args);
    let mut store = Store::new(
        &engine,
        MyState {
            ctx: builder.build(),
            table: ResourceTable::default(),
        },
    );

    let command = Command::instantiate_async(&mut store, &component, &linker).await?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await??;
    match result {
        Ok(()) => Ok(()),
        Err(()) => std::process::exit(1),
    }
}
