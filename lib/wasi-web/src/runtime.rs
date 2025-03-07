use std::time::Duration;
/// ^1: bindgen glue marks its calls as unsafe - namely the use of
///     shared references that can be sent to is not in line with
///     the way the rust borrow checker is meant to work. hence
///     this file has some `unsafe` code in it
use std::{future::Future, io, pin::Pin, sync::Arc, task::Poll};

use futures::future::BoxFuture;
use http::{HeaderMap, StatusCode};
use js_sys::Promise;
use tokio::{
    io::{AsyncRead, AsyncSeek, AsyncWrite},
    runtime::{Builder, Runtime},
    sync::mpsc,
};
#[allow(unused_imports, dead_code)]
use tracing::{debug, error, info, trace, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasmer_wasix::{
    http::{DynHttpClient, HttpRequest, HttpResponse},
    os::{TtyBridge, TtyOptions},
    runtime::{
        module_cache::ModuleCache,
        package_loader::{BuiltinPackageLoader, PackageLoader},
        resolver::{Source, WapmSource},
        task_manager::TaskWasm,
    },
    VirtualFile, VirtualNetworking, VirtualTaskManager, WasiThreadError, WasiTtyState,
};
use web_sys::WebGl2RenderingContext;

#[cfg(feature = "webgl")]
use super::webgl::GlContext;
#[cfg(feature = "webgl")]
use super::webgl::WebGl;
#[cfg(feature = "webgl")]
use super::webgl::WebGlCommand;
use super::{common::*, pool::WebThreadPool};
use crate::module_cache::WebWorkerModuleCache;

#[derive(Debug)]
pub(crate) enum TerminalCommandRx {
    Print(String),
    #[allow(dead_code)]
    Cls,
}

#[derive(Debug)]
pub(crate) struct WebRuntime {
    pub(crate) pool: WebThreadPool,
    #[cfg(feature = "webgl")]
    webgl_tx: mpsc::UnboundedSender<WebGlCommand>,
    tty: TtyOptions,

    http_client: DynHttpClient,
    package_loader: Arc<BuiltinPackageLoader>,
    source: Arc<WapmSource>,
    module_cache: Arc<dyn ModuleCache + Send + Sync>,
    net: wasmer_wasix::virtual_net::DynVirtualNetworking,
    tasks: Arc<dyn VirtualTaskManager>,
}

impl WebRuntime {
    #[allow(unused_variables)]
    pub(crate) fn new(
        pool: WebThreadPool,
        tty_options: TtyOptions,
        webgl2: WebGl2RenderingContext,
    ) -> WebRuntime {
        #[cfg(feature = "webgl")]
        let webgl_tx = GlContext::init(webgl2);

        let runtime: Arc<Runtime> = Arc::new(Builder::new_current_thread().build().unwrap());
        let runtime = Arc::new(WebTaskManager { pool: pool.clone() });

        let http_client = Arc::new(WebHttpClient { pool: pool.clone() });
        let source = WapmSource::new(
            WapmSource::WASMER_PROD_ENDPOINT.parse().unwrap(),
            http_client.clone(),
        );

        let wasmer_dir = std::env::current_dir().unwrap_or_else(|_| ".wasmer".into());

        // Note: the package loader and module cache have tiered caching, so
        // even if the filesystem cache fails (i.e. because we're compiled to
        // wasm32-unknown-unknown and running in a browser), the in-memory layer
        // should still work.
        let package_loader = BuiltinPackageLoader::new_only_client(http_client.clone());
        let module_cache = WebWorkerModuleCache::default();
        WebRuntime {
            pool,
            tasks: runtime,
            tty: tty_options,
            #[cfg(feature = "webgl")]
            webgl_tx,
            http_client,
            net: Arc::new(WebVirtualNetworking),
            module_cache: Arc::new(module_cache),
            package_loader: Arc::new(package_loader),
            source: Arc::new(source),
        }
    }
}

#[derive(Clone, Debug)]
struct WebVirtualNetworking;

impl VirtualNetworking for WebVirtualNetworking {}

#[derive(Debug, Clone)]
pub(crate) struct WebTaskManager {
    pool: WebThreadPool,
}

struct WebRuntimeGuard<'g> {
    #[allow(unused)]
    inner: tokio::runtime::EnterGuard<'g>,
}
impl<'g> Drop for WebRuntimeGuard<'g> {
    fn drop(&mut self) {}
}

#[async_trait::async_trait]
#[allow(unused_variables)]
impl VirtualTaskManager for WebTaskManager {
    /// Invokes whenever a WASM thread goes idle. In some runtimes (like singlethreaded
    /// execution environments) they will need to do asynchronous work whenever the main
    /// thread goes idle and this is the place to hook for that.
    fn sleep_now(
        &self,
        time: Duration,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>> {
        // The async code itself has to be sent to a main JS thread as this is where
        // time can be handled properly - later we can look at running a JS runtime
        // on the dedicated threads but that will require that processes can be unwound
        // using asyncify
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pool.spawn_shared(Box::new(move || {
            Box::pin(async move {
                let time = if time.as_millis() < i32::MAX as u128 {
                    time.as_millis() as i32
                } else {
                    i32::MAX
                };
                let promise = bindgen_sleep(time);
                let js_fut = JsFuture::from(promise);
                let _ = js_fut.await;
                let _ = tx.send(());
            })
        }));
        Box::pin(async move {
            let _ = rx.await;
        })
    }

    /// Starts an asynchronous task that will run on a shared worker pool
    /// This task must not block the execution or it could cause a deadlock
    fn task_shared(
        &self,
        task: Box<
            dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + 'static,
        >,
    ) -> Result<(), WasiThreadError> {
        self.pool.spawn_shared(Box::new(move || {
            Box::pin(async move {
                let fut = task();
                fut.await
            })
        }));
        Ok(())
    }

    /// Starts an asynchronous task will will run on a dedicated thread
    /// pulled from the worker pool that has a stateful thread local variable
    /// It is ok for this task to block execution and any async futures within its scope
    fn task_wasm(&self, task: TaskWasm) -> Result<(), WasiThreadError> {
        self.pool.spawn_wasm(task)?;
        Ok(())
    }

    /// Starts an asynchronous task will will run on a dedicated thread
    /// pulled from the worker pool. It is ok for this task to block execution
    /// and any async futures within its scope
    fn task_dedicated(
        &self,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<(), WasiThreadError> {
        self.pool.spawn_dedicated(task);
        Ok(())
    }
    /// Returns the amount of parallelism that is possible on this platform
    fn thread_parallelism(&self) -> Result<usize, WasiThreadError> {
        Ok(8)
    }
}

#[derive(Debug, Clone)]
pub struct TermStdout {
    term_tx: mpsc::UnboundedSender<TerminalCommandRx>,
    tty: TtyOptions,
}

impl TermStdout {
    pub(crate) fn new(tx: mpsc::UnboundedSender<TerminalCommandRx>, tty: TtyOptions) -> Self {
        Self { term_tx: tx, tty }
    }

    fn term_write(&self, data: &[u8]) {
        let data = match self.tty.line_feeds() {
            true => data
                .to_vec()
                .into_iter()
                .flat_map(|a| match a {
                    b'\n' => vec![b'\r', b'\n'].into_iter(),
                    a => vec![a].into_iter(),
                })
                .collect::<Vec<_>>(),
            false => data.to_vec(),
        };
        if let Ok(text) = String::from_utf8(data) {
            self.term_tx.send(TerminalCommandRx::Print(text)).unwrap();
        }
    }
}

impl AsyncRead for TermStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for TermStdout {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.term_write(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for TermStdout {
    fn start_seek(self: Pin<&mut Self>, _position: io::SeekFrom) -> io::Result<()> {
        Ok(())
    }

    fn poll_complete(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

impl VirtualFile for TermStdout {
    fn last_accessed(&self) -> u64 {
        0
    }

    fn last_modified(&self) -> u64 {
        0
    }

    fn created_time(&self) -> u64 {
        0
    }

    fn size(&self) -> u64 {
        0
    }

    fn set_len(&mut self, _new_size: u64) -> wasmer_wasix::virtual_fs::Result<()> {
        Ok(())
    }

    fn unlink(&mut self) -> BoxFuture<'static, wasmer_wasix::virtual_fs::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn poll_read_ready(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<usize>> {
        Poll::Pending
    }

    fn poll_write_ready(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<usize>> {
        Poll::Ready(Ok(8192))
    }
}

#[derive(Debug, Clone)]
pub struct TermLog {
    pool: WebThreadPool,
}

impl TermLog {
    #[allow(dead_code)]
    pub(crate) fn new(pool: WebThreadPool) -> Self {
        Self { pool }
    }

    fn log_write(&self, data: &[u8]) {
        let text = String::from_utf8_lossy(data).to_string();
        self.pool.spawn_shared(Box::new(move || {
            Box::pin(async move {
                // See ^1 at file header
                #[allow(unused_unsafe)]
                unsafe {
                    console::log(text.as_str())
                };
            })
        }));
    }
}

impl AsyncRead for TermLog {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for TermLog {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.log_write(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for TermLog {
    fn start_seek(self: Pin<&mut Self>, _position: io::SeekFrom) -> io::Result<()> {
        Ok(())
    }

    fn poll_complete(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

impl VirtualFile for TermLog {
    fn last_accessed(&self) -> u64 {
        0
    }

    fn last_modified(&self) -> u64 {
        0
    }

    fn created_time(&self) -> u64 {
        0
    }

    fn size(&self) -> u64 {
        0
    }

    fn set_len(&mut self, _new_size: u64) -> wasmer_wasix::virtual_fs::Result<()> {
        Ok(())
    }

    fn unlink(&mut self) -> BoxFuture<'static, wasmer_wasix::virtual_fs::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn poll_read_ready(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<usize>> {
        Poll::Pending
    }

    fn poll_write_ready(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<usize>> {
        Poll::Ready(Ok(8192))
    }
}

impl wasmer_wasix::Runtime for WebRuntime {
    fn networking(&self) -> &wasmer_wasix::virtual_net::DynVirtualNetworking {
        &self.net
    }

    fn task_manager(&self) -> &Arc<dyn VirtualTaskManager> {
        &self.tasks
    }

    fn tty(&self) -> Option<&(dyn TtyBridge + Send + Sync)> {
        Some(self)
    }

    fn http_client(&self) -> Option<&DynHttpClient> {
        Some(&self.http_client)
    }

    fn module_cache(&self) -> Arc<dyn ModuleCache + Send + Sync> {
        Arc::clone(&self.module_cache)
    }

    fn package_loader(&self) -> Arc<dyn PackageLoader + Send + Sync> {
        Arc::clone(&self.package_loader) as Arc<dyn PackageLoader + Send + Sync>
    }

    fn source(&self) -> Arc<dyn Source + Send + Sync> {
        Arc::clone(&self.source) as Arc<dyn Source + Send + Sync>
    }
}

impl TtyBridge for WebRuntime {
    fn reset(&self) {
        self.tty.set_echo(true);
        self.tty.set_line_buffering(true);
        self.tty.set_line_feeds(true);
    }

    fn tty_get(&self) -> WasiTtyState {
        WasiTtyState {
            cols: self.tty.cols(),
            rows: self.tty.rows(),
            width: 800,
            height: 600,
            stdin_tty: true,
            stdout_tty: true,
            stderr_tty: true,
            echo: self.tty.echo(),
            line_buffered: self.tty.line_buffering(),
            line_feeds: self.tty.line_feeds(),
        }
    }

    fn tty_set(&self, tty_state: WasiTtyState) {
        self.tty.set_cols(tty_state.cols);
        self.tty.set_rows(tty_state.rows);
        self.tty.set_echo(tty_state.echo);
        self.tty.set_line_buffering(tty_state.line_buffered);
        self.tty.set_line_feeds(tty_state.line_feeds);
    }

    /*
    fn cls(&self) -> Pin<Box<dyn Future<Output = io::Result<()>>>> {
        let tx = self.term_tx.clone();
        Box::pin(async move {
            let _ = tx.send(TerminalCommandRx::Cls);
            Ok(())
        })
    }
    */
}

#[derive(Clone, Debug)]
struct WebHttpClient {
    pool: WebThreadPool,
}

impl WebHttpClient {
    async fn do_request(request: HttpRequest) -> Result<HttpResponse, anyhow::Error> {
        let resp = crate::common::fetch(
            request.url.as_str(),
            request.method.as_str(),
            request.options.gzip,
            request.options.cors_proxy,
            &request.headers,
            request.body,
        )
        .await?;

        let redirected = resp.redirected();
        let status = StatusCode::from_u16(resp.status())?;

        let data = crate::common::get_response_data(resp).await?;

        // FIXME: we can't implement this as the method resp.headers().keys() is missing!
        // how else are we going to parse the headers?
        let headers = HeaderMap::new();

        debug!("received {} bytes", data.len());
        let resp = HttpResponse {
            redirected,
            status,
            headers,
            body: Some(data),
        };
        debug!("response status {}", status);

        Ok(resp)
    }
}

impl wasmer_wasix::http::HttpClient for WebHttpClient {
    fn request(
        &self,
        request: wasmer_wasix::http::HttpRequest,
    ) -> BoxFuture<Result<wasmer_wasix::http::HttpResponse, anyhow::Error>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        // The async code itself has to be sent to a main JS thread as this is where
        // HTTP requests can be handled properly - later we can look at running a JS runtime
        // on the dedicated threads but that will require that processes can be unwound
        // using asyncify
        self.pool.spawn_shared(Box::new(move || {
            Box::pin(async move {
                let res = Self::do_request(request).await;
                if let Err(err) = tx.send(res) {
                    tracing::error!("failed to reply http response to caller - {:?}", err);
                }
            })
        }));
        Box::pin(async move { rx.await.unwrap() })
    }
}

#[wasm_bindgen(module = "/js/time.js")]
extern "C" {
    #[wasm_bindgen(js_name = "sleep")]
    pub fn bindgen_sleep(ms: i32) -> Promise;
}
