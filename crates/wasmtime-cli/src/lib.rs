#![allow(clippy::type_complexity)]

use core::iter;
use core::pin::pin;
use core::time::Duration;

use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use clap::Parser;
use futures::StreamExt as _;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{error, info, instrument, warn, Instrument as _, Span};
use url::Url;
use wasi_preview1_component_adapter_provider::{
    WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, WASI_SNAPSHOT_PREVIEW1_COMMAND_ADAPTER,
    WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER,
};
use wasmtime::component::{types, Component, InstancePre, Linker, ResourceType};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{IoView, ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};
use wrpc_runtime_wasmtime::{
    collect_component_resources, link_item, ServeExt as _, SharedResourceTable, WrpcView,
};
use wrpc_transport::{Invoke, Serve};

mod nats;
mod tcp;

const DEFAULT_TIMEOUT: &str = "10s";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
enum Command {
    #[command(subcommand)]
    Nats(nats::Command),
    #[command(subcommand)]
    Tcp(tcp::Command),
}

pub enum Workload {
    Url(Url),
    Binary(Vec<u8>),
}

pub struct Ctx<C: Invoke> {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub http: WasiHttpCtx,
    pub wrpc: C,
    pub cx: C::Context,
    pub shared_resources: SharedResourceTable,
    pub timeout: Duration,
}

impl<C> WrpcView for Ctx<C>
where
    C: Invoke,
    C::Context: Clone,
{
    type Invoke = C;

    fn context(&self) -> C::Context {
        self.cx.clone()
    }

    fn client(&self) -> &Self::Invoke {
        &self.wrpc
    }

    fn shared_resources(&mut self) -> &mut SharedResourceTable {
        &mut self.shared_resources
    }

    fn timeout(&self) -> Option<Duration> {
        Some(self.timeout)
    }
}

impl<C: Invoke> IoView for Ctx<C> {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl<C: Invoke> WasiView for Ctx<C> {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

impl<C: Invoke> WasiHttpView for Ctx<C> {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }
}

// https://github.com/bytecodealliance/wasmtime/blob/b943666650696f1eb7ff8b217762b58d5ef5779d/src/commands/serve.rs#L641-L656
fn use_pooling_allocator_by_default() -> anyhow::Result<Option<bool>> {
    const BITS_TO_TEST: u32 = 42;
    let mut config = wasmtime::Config::new();
    config.wasm_memory64(true);
    config.memory_reservation(1 << BITS_TO_TEST);
    let engine = wasmtime::Engine::new(&config)?;
    let mut store = wasmtime::Store::new(&engine, ());
    // NB: the maximum size is in wasm pages to take out the 16-bits of wasm
    // page size here from the maximum size.
    let ty = wasmtime::MemoryType::new64(0, Some(1 << (BITS_TO_TEST - 16)));
    if wasmtime::Memory::new(&mut store, ty).is_ok() {
        Ok(Some(true))
    } else {
        Ok(None)
    }
}

fn is_0_2(version: &str, min_patch: u64) -> bool {
    if let Ok(semver::Version {
        major,
        minor,
        patch,
        pre,
        build,
    }) = version.parse()
    {
        major == 0 && minor == 2 && patch >= min_patch && pre.is_empty() && build.is_empty()
    } else {
        false
    }
}

#[instrument(level = "trace", skip(adapter))]
async fn instantiate_pre<C>(
    adapter: &[u8],
    workload: &str,
) -> anyhow::Result<(InstancePre<Ctx<C>>, Engine, Arc<[ResourceType]>)>
where
    C: Invoke + Clone + 'static,
    C::Context: Clone + 'static,
{
    let mut opts = wasmtime_cli_flags::CommonOptions::try_parse_from(iter::empty::<&'static str>())
        .context("failed to construct common Wasmtime options")?;
    let mut config = opts
        .config(use_pooling_allocator_by_default().unwrap_or(None))
        .context("failed to construct Wasmtime config")?;
    config.wasm_component_model(true);
    config.async_support(true);
    let engine = wasmtime::Engine::new(&config).context("failed to initialize Wasmtime engine")?;

    let wasm = if workload.starts_with('.') || workload.starts_with('/') {
        fs::read(&workload)
            .await
            .with_context(|| format!("failed to read relative path to workload `{workload}`"))
            .map(Workload::Binary)
    } else {
        Url::parse(workload)
            .with_context(|| format!("failed to parse Wasm URL `{workload}`"))
            .map(Workload::Url)
    }?;
    let wasm = match wasm {
        Workload::Url(wasm) => match wasm.scheme() {
            "file" => {
                let wasm = wasm
                    .to_file_path()
                    .map_err(|()| anyhow!("failed to convert Wasm URL to file path"))?;
                fs::read(wasm)
                    .await
                    .context("failed to read Wasm from file URL")?
            }
            "http" | "https" => {
                let wasm = reqwest::get(wasm).await.context("failed to GET Wasm URL")?;
                let wasm = wasm.bytes().await.context("failed fetch Wasm from URL")?;
                wasm.to_vec()
            }
            scheme => bail!("URL scheme `{scheme}` not supported"),
        },
        Workload::Binary(wasm) => wasm,
    };
    let wasm = if wasmparser::Parser::is_core_wasm(&wasm) {
        wit_component::ComponentEncoder::default()
            .validate(true)
            .module(&wasm)
            .context("failed to set core component module")?
            .adapter(WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, adapter)
            .context("failed to add WASI adapter")?
            .encode()
            .context("failed to encode a component")?
    } else {
        wasm
    };

    let component = Component::new(&engine, wasm).context("failed to compile component")?;

    let mut linker = Linker::<Ctx<C>>::new(&engine);
    wasmtime_wasi::add_to_linker_async(&mut linker).context("failed to link WASI")?;
    wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)
        .context("failed to link `wasi:http`")?;
    wrpc_runtime_wasmtime::rpc::add_to_linker(&mut linker).context("failed to link `wrpc:rpc`")?;

    let ty = component.component_type();
    let mut resources = Vec::new();
    collect_component_resources(&engine, &ty, &mut resources);
    let resources = Arc::from(resources);
    for (name, item) in ty.imports(&engine) {
        // Avoid polyfilling instances, for which static bindings are linked
        match name.split_once('/').map(|(pkg, suffix)| {
            suffix
                .split_once('@')
                .map_or((pkg, suffix, None), |(iface, version)| {
                    (pkg, iface, Some(version))
                })
        }) {
            Some(("wrpc:rpc", "transport" | "error" | "context" | "invoker", Some("0.1.0"))) => {}
            Some((
                "wasi:cli",
                "environment" | "exit" | "stderr" | "stdin" | "stdout" | "terminal-input"
                | "terminal-output" | "terminal-stderr" | "terminal-stdin" | "terminal-stdout",
                Some(version),
            )) if is_0_2(version, 0) => {}
            Some(("wasi:clocks", "monotonic-clock" | "wall-clock", Some(version)))
                if is_0_2(version, 0) => {}
            Some(("wasi:clocks", "timezone", Some(version))) if is_0_2(version, 1) => {}
            Some(("wasi:filesystem", "preopens" | "types", Some(version)))
                if is_0_2(version, 0) => {}
            Some((
                "wasi:http",
                "incoming-handler" | "outgoing-handler" | "types",
                Some(version),
            )) if is_0_2(version, 0) => {}
            Some(("wasi:io", "error" | "poll" | "streams", Some(version)))
                if is_0_2(version, 0) => {}
            Some(("wasi:random", "insecure-seed" | "insecure" | "random", Some(version)))
                if is_0_2(version, 0) => {}
            Some((
                "wasi:sockets",
                "instance-network" | "ip-name-lookup" | "network" | "tcp-create-socket" | "tcp"
                | "udp-create-socket" | "udp",
                Some(version),
            )) if is_0_2(version, 0) => {}
            _ => {
                if let Err(err) = link_item(
                    &engine,
                    &mut linker.root(),
                    Arc::clone(&resources),
                    item,
                    "",
                    name,
                ) {
                    error!(?err, "failed to polyfill instance");
                }
            }
        }
    }

    let pre = linker
        .instantiate_pre(&component)
        .context("failed to pre-instantiate component")?;
    Ok((pre, engine, resources))
}

fn new_store<C: Invoke>(
    engine: &Engine,
    wrpc: C,
    cx: C::Context,
    arg0: &str,
    timeout: Duration,
) -> wasmtime::Store<Ctx<C>> {
    Store::new(
        engine,
        Ctx {
            wasi: WasiCtxBuilder::new()
                .inherit_env()
                .inherit_stdio()
                .inherit_network()
                .allow_ip_name_lookup(true)
                .allow_tcp(true)
                .allow_udp(true)
                .args(&[arg0])
                .build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            shared_resources: SharedResourceTable::default(),
            wrpc,
            cx,
            timeout,
        },
    )
}

#[instrument(level = "trace", skip(clt, cx), ret(level = "trace"))]
pub async fn handle_run<C>(
    clt: C,
    cx: C::Context,
    timeout: Duration,
    workload: &str,
) -> anyhow::Result<()>
where
    C: Invoke + Clone + 'static,
    C::Context: Clone + 'static,
{
    let (pre, engine, _) =
        instantiate_pre(WASI_SNAPSHOT_PREVIEW1_COMMAND_ADAPTER, workload).await?;
    let mut store = new_store(&engine, clt, cx, "command.wasm", timeout);
    let cmd = wasmtime_wasi::bindings::CommandPre::new(pre)
        .context("failed to construct `command` instance")?
        .instantiate_async(&mut store)
        .await
        .context("failed to instantiate `command`")?;
    cmd.wasi_cli_run()
        .call_run(&mut store)
        .await
        .context("failed to run component")?
        .map_err(|()| anyhow!("component failed"))
}

#[instrument(level = "trace", skip_all, ret(level = "trace"))]
pub async fn serve_shared<C, S>(
    handlers: &mut JoinSet<()>,
    srv: S,
    mut store: wasmtime::Store<Ctx<C>>,
    pre: InstancePre<Ctx<C>>,
    guest_resources: Arc<[ResourceType]>,
) -> anyhow::Result<()>
where
    C: Invoke + 'static,
    C::Context: Clone,
    S: Serve,
{
    let span = Span::current();
    let instance = pre
        .instantiate_async(&mut store)
        .await
        .context("failed to instantiate component")?;
    let engine = store.engine().clone();
    let store = Arc::new(Mutex::new(store));
    for (name, ty) in pre.component().component_type().exports(&engine) {
        match (name, ty) {
            (name, types::ComponentItem::ComponentFunc(ty)) => {
                info!(?name, "serving root function");
                let invocations = srv
                    .serve_function_shared(
                        Arc::clone(&store),
                        instance,
                        Arc::clone(&guest_resources),
                        ty,
                        "",
                        name,
                    )
                    .await?;
                handlers.spawn(
                    async move {
                        let mut invocations = pin!(invocations);
                        while let Some(invocation) = invocations.next().await {
                            match invocation {
                                Ok((_, fut)) => {
                                    info!("serving root function invocation");
                                    if let Err(err) = fut.await {
                                        warn!(?err, "failed to serve root function invocation");
                                    } else {
                                        info!("successfully served root function invocation");
                                    }
                                }
                                Err(err) => {
                                    error!(?err, "failed to accept root function invocation");
                                }
                            }
                        }
                    }
                    .instrument(span.clone()),
                );
            }
            (_, types::ComponentItem::CoreFunc(_)) => {
                warn!(name, "serving root core function exports not supported yet");
            }
            (_, types::ComponentItem::Module(_)) => {
                warn!(name, "serving root module exports not supported yet");
            }
            (_, types::ComponentItem::Component(_)) => {
                warn!(name, "serving root component exports not supported yet");
            }
            (instance_name, types::ComponentItem::ComponentInstance(ty)) => {
                for (name, ty) in ty.exports(&engine) {
                    match ty {
                        types::ComponentItem::ComponentFunc(ty) => {
                            info!(?name, "serving instance function");
                            let invocations = srv
                                .serve_function_shared(
                                    Arc::clone(&store),
                                    instance,
                                    Arc::clone(&guest_resources),
                                    ty,
                                    instance_name,
                                    name,
                                )
                                .await?;
                            handlers.spawn(async move {
                                let mut invocations = pin!(invocations);
                                while let Some(invocation) = invocations.next().await {
                                    match invocation {
                                        Ok((_, fut)) => {
                                            info!("serving instance function invocation");
                                            if let Err(err) = fut.await {
                                                warn!(
                                                    ?err,
                                                    "failed to serve instance function invocation"
                                                );
                                            } else {
                                                info!(
                                                    "successfully served instance function invocation"
                                                );
                                            }
                                        }
                                        Err(err) => {
                                            error!(
                                                ?err,
                                                "failed to accept instance function invocation"
                                            );
                                        }
                                    }
                                }
                            }
                            .instrument(span.clone()));
                        }
                        types::ComponentItem::CoreFunc(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance core function exports not supported yet"
                            );
                        }
                        types::ComponentItem::Module(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance module exports not supported yet"
                            );
                        }
                        types::ComponentItem::Component(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance component exports not supported yet"
                            );
                        }
                        types::ComponentItem::ComponentInstance(_) => {
                            warn!(
                                instance_name,
                                name, "serving nested instance exports not supported yet"
                            );
                        }
                        types::ComponentItem::Type(_) | types::ComponentItem::Resource(_) => {}
                    }
                }
            }
            (_, types::ComponentItem::Type(_) | types::ComponentItem::Resource(_)) => {}
        }
    }
    Ok(())
}

#[instrument(level = "trace", skip_all, ret(level = "trace"))]
pub async fn serve_stateless<C, S>(
    handlers: &mut JoinSet<()>,
    srv: S,
    clt: C,
    cx: C::Context,
    pre: InstancePre<Ctx<C>>,
    engine: &Engine,
    timeout: Duration,
) -> anyhow::Result<()>
where
    C: Invoke + Clone + 'static,
    C::Context: Clone + 'static,
    S: Serve,
{
    let span = Span::current();
    for (name, ty) in pre.component().component_type().exports(engine) {
        match (name, ty) {
            (name, types::ComponentItem::ComponentFunc(ty)) => {
                let clt = clt.clone();
                let cx = cx.clone();
                let engine = engine.clone();
                info!(?name, "serving root function");
                let invocations = srv
                    .serve_function(
                        move || {
                            new_store(&engine, clt.clone(), cx.clone(), "reactor.wasm", timeout)
                        },
                        pre.clone(),
                        ty,
                        "",
                        name,
                    )
                    .await?;
                handlers.spawn(
                    async move {
                        let mut invocations = pin!(invocations);
                        while let Some(invocation) = invocations.next().await {
                            match invocation {
                                Ok((_, fut)) => {
                                    info!("serving root function invocation");
                                    if let Err(err) = fut.await {
                                        warn!(?err, "failed to serve root function invocation");
                                    } else {
                                        info!("successfully served root function invocation");
                                    }
                                }
                                Err(err) => {
                                    error!(?err, "failed to accept root function invocation");
                                }
                            }
                        }
                    }
                    .instrument(span.clone()),
                );
            }
            (_, types::ComponentItem::CoreFunc(_)) => {
                warn!(name, "serving root core function exports not supported yet");
            }
            (_, types::ComponentItem::Module(_)) => {
                warn!(name, "serving root module exports not supported yet");
            }
            (_, types::ComponentItem::Component(_)) => {
                warn!(name, "serving root component exports not supported yet");
            }
            (instance_name, types::ComponentItem::ComponentInstance(ty)) => {
                for (name, ty) in ty.exports(engine) {
                    match ty {
                        types::ComponentItem::ComponentFunc(ty) => {
                            let clt = clt.clone();
                            let engine = engine.clone();
                            let cx = cx.clone();
                            info!(?name, "serving instance function");
                            let invocations = srv
                                .serve_function(
                                    move || {
                                        new_store(
                                            &engine,
                                            clt.clone(),
                                            cx.clone(),
                                            "reactor.wasm",
                                            timeout,
                                        )
                                    },
                                    pre.clone(),
                                    ty,
                                    instance_name,
                                    name,
                                )
                                .await?;
                            handlers.spawn(async move {
                                let mut invocations = pin!(invocations);
                                while let Some(invocation) = invocations.next().await {
                                    match invocation {
                                        Ok((_, fut)) => {
                                            info!("serving instance function invocation");
                                            if let Err(err) = fut.await {
                                                warn!(
                                                    ?err,
                                                    "failed to serve instance function invocation"
                                                );
                                            } else {
                                                info!(
                                                    "successfully served instance function invocation"
                                                );
                                            }
                                        }
                                        Err(err) => {
                                            error!(
                                                ?err,
                                                "failed to accept instance function invocation"
                                            );
                                        }
                                    }
                                }
                            }.instrument(span.clone()));
                        }
                        types::ComponentItem::CoreFunc(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance core function exports not supported yet"
                            );
                        }
                        types::ComponentItem::Module(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance module exports not supported yet"
                            );
                        }
                        types::ComponentItem::Component(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance component exports not supported yet"
                            );
                        }
                        types::ComponentItem::ComponentInstance(_) => {
                            warn!(
                                instance_name,
                                name, "serving nested instance exports not supported yet"
                            );
                        }
                        types::ComponentItem::Type(_) | types::ComponentItem::Resource(_) => {}
                    }
                }
            }
            (_, types::ComponentItem::Type(_) | types::ComponentItem::Resource(_)) => {}
        }
    }
    Ok(())
}

#[instrument(level = "trace", skip(srv, clt, cx), ret(level = "trace"))]
pub async fn handle_serve<C, S>(
    srv: S,
    clt: C,
    cx: C::Context,
    timeout: Duration,
    workload: &str,
) -> anyhow::Result<()>
where
    C: Invoke + Clone + 'static,
    C::Context: Clone + 'static,
    S: Serve,
{
    let (pre, engine, guest_resources) =
        instantiate_pre(WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER, workload).await?;

    let mut handlers = JoinSet::new();
    if guest_resources.is_empty() {
        serve_stateless(&mut handlers, srv, clt, cx, pre, &engine, timeout).await?;
    } else {
        serve_shared(
            &mut handlers,
            srv,
            new_store(&engine, clt, cx, "reactor.wasm", timeout),
            pre,
            guest_resources,
        )
        .await?;
    }
    while let Some(res) = handlers.join_next().await {
        if let Err(err) = res {
            error!(?err, "handler failed");
        }
    }
    Ok(())
}

#[instrument(level = "trace", ret(level = "trace"))]
pub async fn run() -> anyhow::Result<()> {
    wrpc_cli::tracing::init();
    match Command::parse() {
        Command::Nats(args) => nats::run(args).await,
        Command::Tcp(args) => tcp::run(args).await,
    }
}
