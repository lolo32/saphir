//! Server is the centerpiece on saphir, it contains everything to handle
//! request and dispatch it the proper router
//!
//! *SAFETY NOTICE*
//!
//! To allow controller and middleware to respond future with static lifetime,
//! the server stack is put inside a static variable. This is needed for safety,
//! but also means that only one saphir server can run at a time

use std::{future::Future, mem::MaybeUninit, net::SocketAddr};

use futures::{
    prelude::*,
    stream::StreamExt,
    task::{Context, Poll},
};
use hyper::{body::Body as RawBody, server::conn::Http, service::Service};
use parking_lot::{Once, OnceState};
use tokio::net::TcpListener;

use crate::{
    body::Body,
    error::SaphirError,
    http_context::HttpContext,
    middleware::{Builder as MiddlewareStackBuilder, MiddleChainEnd, MiddlewareChain},
    request::Request,
    response::Response,
    router::{Builder as RouterBuilder, Router, RouterChain, RouterChainEnd},
};
use futures::future::pending;
use http::{HeaderValue, Request as RawRequest, Response as RawResponse};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

/// Default time for request handling is 30 seconds
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
/// Default listener ip addr is AnyAddr (0.0.0.0)
pub const DEFAULT_LISTENER_IFACE: &str = "0.0.0.0:0";
pub const DEFAULT_SERVER_NAME: &str = "Saphir";

#[doc(hidden)]
static mut STACK: MaybeUninit<Stack> = MaybeUninit::uninit();
#[doc(hidden)]
static mut SERVER_NAME: MaybeUninit<HeaderValue> = MaybeUninit::uninit();
#[doc(hidden)]
static INIT_STACK: Once = Once::new();
#[doc(hidden)]
static REQUEST_FUTURE_COUNT: AtomicU64 = AtomicU64::new(0);

fn write_into_static(stack: Stack, server_value: HeaderValue, request_body_max: Option<usize>) -> Result<&'static Stack, SaphirError> {
    if INIT_STACK.state() != OnceState::New {
        return Err(SaphirError::StackAlreadyInitialized);
    }

    INIT_STACK.call_once(|| {
        // # SAFETY #
        // We write only once in the static memory. No override.
        // Above check also make sure there is no second server.
        unsafe {
            STACK.as_mut_ptr().write(stack);
            SERVER_NAME.as_mut_ptr().write(server_value);
            crate::body::REQUEST_BODY_BYTES_LIMIT = request_body_max;
        }
    });

    // # SAFETY #
    // Memory has been initialized above.
    let stack = unsafe { STACK.as_ptr().as_ref().expect("Memory has been initialized above.") };

    Ok(stack)
}

/// Using Feature `https`
///
/// A struct representing certificate or private key configuration.
#[cfg(feature = "https")]
#[derive(Clone)]
pub enum SslConfig {
    /// File path
    FilePath(String),

    /// File content where all \n and space have been removed.
    FileData(String),
}

#[derive(Default)]
pub struct ListenerBuilder {
    iface: Option<String>,
    server_name: Option<String>,
    request_timeout_ms: Option<u64>,
    request_body_max: Option<usize>,
    #[cfg(feature = "https")]
    cert_config: Option<SslConfig>,
    #[cfg(feature = "https")]
    key_config: Option<SslConfig>,
    shutdown_signal: Option<Box<dyn Future<Output = ()> + Unpin + Send + 'static>>,
    graceful_shutdown: bool,
}

impl ListenerBuilder {
    #[inline]
    pub fn new() -> Self {
        #[cfg(not(feature = "https"))]
        {
            ListenerBuilder {
                iface: None,
                request_timeout_ms: Some(DEFAULT_REQUEST_TIMEOUT_MS),
                ..Default::default()
            }
        }
        #[cfg(feature = "https")]
        {
            ListenerBuilder {
                iface: None,
                request_timeout_ms: Some(DEFAULT_REQUEST_TIMEOUT_MS),
                ..Default::default()
            }
        }
    }

    #[inline]
    pub fn interface(mut self, s: &str) -> Self {
        self.iface = Some(s.to_string());
        self
    }

    #[inline]
    pub fn request_timeout<T: Into<Option<u64>>>(mut self, timeout_ms: T) -> Self {
        self.request_timeout_ms = timeout_ms.into();
        self
    }

    #[inline]
    pub fn server_name(mut self, name: &str) -> Self {
        self.server_name = Some(name.to_string());
        self
    }

    #[inline]
    pub fn request_body_max_bytes<I: Into<Option<usize>>>(mut self, size: I) -> Self {
        self.request_body_max = size.into();
        self
    }

    /// Set a shutdown signal to terminate the server.
    ///
    /// If `graceful` is set to `true`, the server will wait for all ongoing
    /// request to be completed before shutting down but will stop accepting
    /// new requests.
    #[inline]
    pub fn shutdown<F: Future<Output = ()> + Unpin + Send + 'static>(mut self, signal: F, graceful: bool) -> Self {
        self.shutdown_signal = Some(Box::new(signal));
        self.graceful_shutdown = graceful;
        self
    }

    /// Using Feature `https`
    ///
    /// Set the listener ssl certificates files. The cert needs to be PEM
    /// encoded while the key can be either RSA or PKCS8
    #[inline]
    #[cfg(feature = "https")]
    pub fn set_ssl_certificates(self, cert_path: &str, key_path: &str) -> Self {
        self.set_ssl_config(SslConfig::FilePath(cert_path.to_string()), SslConfig::FilePath(key_path.to_string()))
    }

    /// Using Feature `https`
    ///
    /// Set the listener ssl config. The cert needs to be PEM encoded
    /// while the key can be either RSA or PKCS8. The file path can be used or
    /// the file content directly where all \n and space have been removed.
    #[inline]
    #[cfg(feature = "https")]
    pub fn set_ssl_config(mut self, cert_config: SslConfig, key_config: SslConfig) -> Self {
        self.cert_config = Some(cert_config);
        self.key_config = Some(key_config);
        self
    }

    #[cfg(feature = "https")]
    #[inline]
    pub(crate) fn build(self) -> ListenerConfig {
        let ListenerBuilder {
            iface,
            server_name,
            request_timeout_ms,
            request_body_max,
            cert_config,
            key_config,
            shutdown_signal,
            graceful_shutdown,
        } = self;

        let iface = iface.unwrap_or_else(|| DEFAULT_LISTENER_IFACE.to_string());
        let shutdown = if let Some(sig) = shutdown_signal {
            ServerShutdown::new(graceful_shutdown, sig)
        } else {
            ServerShutdown::pending()
        };

        ListenerConfig {
            iface,
            request_timeout_ms,
            server_name: server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string()),
            request_body_max,
            cert_config,
            key_config,
            shutdown,
        }
    }

    #[cfg(not(feature = "https"))]
    #[doc(hidden)]
    #[inline]
    pub(crate) fn build(self) -> ListenerConfig {
        let ListenerBuilder {
            iface,
            server_name,
            request_timeout_ms,
            request_body_max,
            shutdown_signal,
            graceful_shutdown,
        } = self;

        let iface = iface.unwrap_or_else(|| DEFAULT_LISTENER_IFACE.to_string());
        let shutdown = if let Some(sig) = shutdown_signal {
            ServerShutdown::new(graceful_shutdown, sig)
        } else {
            ServerShutdown::pending()
        };

        ListenerConfig {
            iface,
            request_timeout_ms,
            server_name: server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string()),
            request_body_max,
            shutdown,
        }
    }
}

#[cfg(feature = "https")]
pub struct ListenerConfig {
    iface: String,
    request_timeout_ms: Option<u64>,
    request_body_max: Option<usize>,
    server_name: String,
    cert_config: Option<SslConfig>,
    key_config: Option<SslConfig>,
    shutdown: ServerShutdown,
}

#[cfg(not(feature = "https"))]
pub struct ListenerConfig {
    iface: String,
    request_timeout_ms: Option<u64>,
    request_body_max: Option<usize>,
    server_name: String,
    shutdown: ServerShutdown,
}

#[cfg(feature = "https")]
impl ListenerConfig {
    pub(crate) fn ssl_config(&self) -> (Option<&SslConfig>, Option<&SslConfig>) {
        (self.cert_config.as_ref(), self.key_config.as_ref())
    }
}

pub struct Builder<Controllers, Middlewares>
where
    Controllers: 'static + RouterChain + Unpin + Send + Sync,
    Middlewares: 'static + MiddlewareChain + Unpin + Send + Sync,
{
    listener: Option<ListenerBuilder>,
    router: RouterBuilder<Controllers>,
    middlewares: MiddlewareStackBuilder<Middlewares>,
}

impl<Controllers, Middlewares> Builder<Controllers, Middlewares>
where
    Controllers: 'static + RouterChain + Unpin + Send + Sync,
    Middlewares: 'static + MiddlewareChain + Unpin + Send + Sync,
{
    #[inline]
    pub fn configure_listener<F>(mut self, f: F) -> Self
    where
        F: FnOnce(ListenerBuilder) -> ListenerBuilder,
    {
        let l = if let Some(builder) = self.listener.take() {
            builder
        } else {
            ListenerBuilder::new()
        };

        self.listener = Some(f(l));

        self
    }

    #[inline]
    pub fn configure_router<F, NewChain: RouterChain + Unpin + Send + Sync>(self, f: F) -> Builder<NewChain, Middlewares>
    where
        F: FnOnce(RouterBuilder<Controllers>) -> RouterBuilder<NewChain>,
    {
        Builder {
            listener: self.listener,
            router: f(self.router),
            middlewares: self.middlewares,
        }
    }

    #[inline]
    pub fn configure_middlewares<F, NewChain: MiddlewareChain + Unpin + Send + Sync>(self, f: F) -> Builder<Controllers, NewChain>
    where
        F: FnOnce(MiddlewareStackBuilder<Middlewares>) -> MiddlewareStackBuilder<NewChain>,
    {
        Builder {
            listener: self.listener,
            router: self.router,
            middlewares: f(self.middlewares),
        }
    }

    pub fn build(self) -> Server {
        Server {
            listener_config: self.listener.unwrap_or_else(ListenerBuilder::new).build(),
            stack: Stack {
                router: self.router.build(),
                middlewares: self.middlewares.build(),
            },
        }
    }

    #[doc(hidden)]
    pub fn build_stack_only(self) -> Result<(), SaphirError> {
        let stack = Stack {
            router: self.router.build(),
            middlewares: self.middlewares.build(),
        };

        let (server_name, request_body_max) = if let Some(listener_builder) = self.listener {
            (listener_builder.server_name, listener_builder.request_body_max)
        } else {
            (None, None)
        };

        let server_value = HeaderValue::from_str(&server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string()))?;

        write_into_static(stack, server_value, request_body_max)?;

        Ok(())
    }
}

#[derive(Default)]
struct SeverShutdownState {
    draining: AtomicBool,
}

impl SeverShutdownState {
    #[inline]
    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }
}

struct ServerShutdown {
    graceful: bool,
    state: Arc<SeverShutdownState>,
    signal: Pin<Box<dyn Future<Output = ()> + Unpin + Send + 'static>>,
}

impl ServerShutdown {
    pub fn new<F: Future<Output = ()> + Unpin + Send + 'static>(graceful: bool, signal: F) -> Self {
        ServerShutdown {
            graceful,
            state: Arc::new(Default::default()),
            signal: Box::pin(signal),
        }
    }

    pub fn pending() -> Self {
        ServerShutdown {
            graceful: false,
            state: Arc::new(Default::default()),
            signal: Box::pin(pending()),
        }
    }
}

impl Future for ServerShutdown {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.draining() {
            let count = REQUEST_FUTURE_COUNT.load(Ordering::SeqCst);
            if count == 0 {
                Poll::Ready(())
            } else {
                let waker = cx.waker().clone();
                tokio::spawn(tokio::time::delay_for(Duration::from_millis(100)).map(move |_| waker.wake()));
                Poll::Pending
            }
        } else {
            match Pin::as_mut(&mut self.signal).poll(cx) {
                Poll::Ready(()) => {
                    if !self.graceful {
                        Poll::Ready(())
                    } else {
                        self.state.draining.store(true, Ordering::SeqCst);
                        let waker = cx.waker().clone();
                        tokio::spawn(tokio::time::delay_for(Duration::from_secs(1)).map(move |_| waker.wake()));
                        Poll::Pending
                    }
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

struct ServerFuture<I, S> {
    incoming: Pin<Box<I>>,
    shutdown: Pin<Box<S>>,
}

impl<I, S> ServerFuture<I, S> {
    pub fn new(incoming: I, shutdown: S) -> Self {
        ServerFuture {
            incoming: Box::pin(incoming),
            shutdown: Box::pin(shutdown),
        }
    }
}

impl<I, S> Future for ServerFuture<I, S>
where
    I: Future<Output = ()>,
    S: Future<Output = ()>,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::as_mut(&mut self.shutdown).poll(cx) {
            Poll::Ready(()) => Poll::Ready(()),
            Poll::Pending => match Pin::as_mut(&mut self.incoming).poll(cx) {
                Poll::Ready(()) => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

pub struct Server {
    listener_config: ListenerConfig,
    stack: Stack,
}

impl Server {
    /// Produce a server builder
    #[inline]
    pub fn builder() -> Builder<RouterChainEnd, MiddleChainEnd> {
        Builder {
            listener: None,
            router: RouterBuilder::default(),
            middlewares: MiddlewareStackBuilder::default(),
        }
    }

    /// Return a future with will run the server. Simply run this future inside
    /// the tokio executor or await it in a async context
    pub async fn run(self) -> Result<(), SaphirError> {
        let Server { listener_config, stack } = self;
        let server_value = HeaderValue::from_str(&listener_config.server_name)?;
        let request_body_max = listener_config.request_body_max;

        let stack = write_into_static(stack, server_value, request_body_max)?;

        let http = Http::new();

        let mut listener = TcpListener::bind(listener_config.iface.clone()).await?;
        let local_addr = listener.local_addr()?;

        let incoming = {
            #[cfg(feature = "https")]
            {
                use crate::server::ssl_loading_utils::MaybeTlsAcceptor;
                match listener_config.ssl_config() {
                    (Some(cert_config), Some(key_config)) => {
                        use crate::server::ssl_loading_utils::*;
                        use tokio_rustls::TlsAcceptor;

                        let certs = load_certs(&cert_config);
                        let key = load_private_key(&key_config);
                        let mut cfg = ::rustls::ServerConfig::new(::rustls::NoClientAuth::new());
                        let _ = cfg.set_single_cert(certs, key);
                        let arc_config = Arc::new(cfg);

                        let acceptor = TlsAcceptor::from(arc_config);

                        let inc = listener.incoming().and_then(move |stream| acceptor.accept(stream));

                        info!("Saphir started and listening on : https://{}", local_addr);

                        MaybeTlsAcceptor::Tls(Box::pin(inc))
                    }
                    (cert_config, key_config) if cert_config.xor(key_config).is_some() => {
                        return Err(SaphirError::Other("Invalid SSL configuration, missing cert or key".to_string()));
                    }
                    _ => {
                        let incoming = listener.incoming();
                        info!("{} started and listening on : http://{}", &listener_config.server_name, local_addr);
                        MaybeTlsAcceptor::Plain(Box::pin(incoming))
                    }
                }
            }

            #[cfg(not(feature = "https"))]
            {
                info!("{} started and listening on : http://{}", &listener_config.server_name, local_addr);
                listener.incoming()
            }
        };

        let shutdown = listener_config.shutdown;
        let state = shutdown.state.clone();

        if let Some(timeout_ms) = listener_config.request_timeout_ms {
            let inc = incoming.for_each_concurrent(None, |client_socket| async {
                if !state.draining() {
                    match client_socket {
                        Ok(client_socket) => {
                            let peer_addr = client_socket.peer_addr().ok();
                            let http = http.clone();
                            tokio::spawn(async move {
                                if let Err(e) = http.serve_connection(client_socket, stack.new_timeout_handler(timeout_ms, peer_addr)).await {
                                    error!("An error occurred while treating a request: {:?}", e);
                                }
                            });
                        }
                        Err(e) => {
                            warn!("incoming connection encountered an error: {}", e);
                        }
                    }
                } else {
                    debug!("Skipping incoming connection due to shutdown");
                }
            });
            ServerFuture::new(inc, shutdown).await;
        } else {
            let inc = incoming.for_each_concurrent(None, |client_socket| async {
                if !state.draining() {
                    match client_socket {
                        Ok(client_socket) => {
                            let peer_addr = client_socket.peer_addr().ok();
                            let http = http.clone();
                            tokio::spawn(async move {
                                if let Err(e) = http.serve_connection(client_socket, stack.new_handler(peer_addr)).await {
                                    error!("An error occurred while treating a request: {:?}", e);
                                }
                            });
                        }
                        Err(e) => {
                            warn!("incoming connection encountered an error: {}", e);
                        }
                    }
                } else {
                    debug!("Skipping incoming connection due to shutdown");
                }
            });
            ServerFuture::new(inc, shutdown).await;
        }

        Ok(())
    }
}

#[doc(hidden)]
pub struct Stack {
    router: Router,
    middlewares: Box<dyn MiddlewareChain>,
}
unsafe impl Send for Stack {}
unsafe impl Sync for Stack {}

impl Stack {
    fn new_handler(&'static self, peer_addr: Option<SocketAddr>) -> StackHandler {
        StackHandler { stack: self, peer_addr }
    }

    fn new_timeout_handler(&'static self, timeout_ms: u64, peer_addr: Option<SocketAddr>) -> TimeoutStackHandler {
        TimeoutStackHandler {
            timeout_ms,
            stack: self,
            peer_addr,
        }
    }

    async fn invoke(&self, mut req: Request<Body>) -> Result<Response<Body>, SaphirError> {
        let meta = self.router.resolve_metadata(&mut req);
        let ctx = HttpContext::new(req, self.router.clone(), meta);
        let err_ctx = ctx.clone_with_empty_state();

        let res = self
            .middlewares
            .next(ctx)
            .await
            .and_then(|mut ctx| ctx.state.take_response().ok_or(SaphirError::ResponseMoved))
            .or_else(|e| {
                let builder = crate::response::Builder::new();
                e.log(&err_ctx);
                e.response_builder(builder, &err_ctx).build().map_err(|e2| {
                    e2.log(&err_ctx);
                    e2
                })
            });
        REQUEST_FUTURE_COUNT.fetch_sub(1, Ordering::SeqCst);
        res
    }

    async fn invoke_with_timeout(&self, mut req: Request<Body>, timeout_ms: u64) -> Result<Response<Body>, SaphirError> {
        use tokio::time::timeout;

        let meta = self.router.resolve_metadata(&mut req);
        let ctx = HttpContext::new(req, self.router.clone(), meta);
        let err_ctx = ctx.clone_with_empty_state();

        let res = match timeout(Duration::from_millis(timeout_ms), async move {
            self.middlewares
                .next(ctx)
                .await
                .and_then(|mut ctx| ctx.state.take_response().ok_or(SaphirError::ResponseMoved))
        })
        .map_err(|_| SaphirError::RequestTimeout)
        .await
        {
            Ok(res) => res,
            Err(e) => {
                let builder = crate::response::Builder::new();
                e.log(&err_ctx);
                e.response_builder(builder, &err_ctx).build().map_err(|e2| {
                    e2.log(&err_ctx);
                    e2
                })
            }
        };
        REQUEST_FUTURE_COUNT.fetch_sub(1, Ordering::SeqCst);
        res
    }
}

type StackHandlerFut<S, E> = dyn Future<Output = Result<S, E>> + Send;

#[doc(hidden)]
#[derive(Clone)]
pub struct StackHandler {
    stack: &'static Stack,
    peer_addr: Option<SocketAddr>,
}

impl Service<hyper::Request<hyper::Body>> for StackHandler {
    type Error = SaphirError;
    type Future = Pin<Box<StackHandlerFut<Self::Response, Self::Error>>>;
    type Response = hyper::Response<hyper::Body>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Request<hyper::Body>) -> Self::Future {
        REQUEST_FUTURE_COUNT.fetch_add(1, Ordering::SeqCst);
        let req = Request::new(req.map(Body::from_raw), self.peer_addr.take());
        Box::pin(self.stack.invoke(req).map(|r| {
            r.and_then(|mut r| {
                // # SAFETY #
                // Memory has been initialized at server startup.
                r.headers_mut().insert(http::header::SERVER, unsafe {
                    SERVER_NAME.as_ptr().as_ref().expect("Memory has been initialized at server startup.").clone()
                });
                r.into_raw().map(|r| r.map(|b| b.into_raw()))
            })
        })) as Self::Future
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub struct TimeoutStackHandler {
    stack: &'static Stack,
    timeout_ms: u64,
    peer_addr: Option<SocketAddr>,
}

impl Service<hyper::Request<hyper::Body>> for TimeoutStackHandler {
    type Error = SaphirError;
    type Future = Pin<Box<StackHandlerFut<Self::Response, Self::Error>>>;
    type Response = hyper::Response<hyper::Body>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Request<hyper::Body>) -> Self::Future {
        REQUEST_FUTURE_COUNT.fetch_add(1, Ordering::SeqCst);
        let req = Request::new(req.map(Body::from_raw), self.peer_addr.take());
        Box::pin(self.stack.invoke_with_timeout(req, self.timeout_ms).map(|r| {
            r.and_then(|mut r| {
                // # SAFETY #
                // Memory has been initialized at server startup.
                r.headers_mut().insert(http::header::SERVER, unsafe {
                    SERVER_NAME.as_ptr().as_ref().expect("Memory has been initialized at server startup.").clone()
                });
                r.into_raw().map(|r| r.map(|b| b.into_raw()))
            })
        })) as Self::Future
    }
}

#[doc(hidden)]
#[cfg(feature = "https")]
mod ssl_loading_utils {
    use std::{fs, io::BufReader, net::SocketAddr, pin::Pin};

    use futures::io::Error;
    use futures_util::{
        stream::Stream,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncWrite};

    use crate::server::SslConfig;

    pub enum MaybeTlsStream {
        Tls(Pin<Box<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>>),
        Plain(Pin<Box<tokio::net::TcpStream>>),
    }

    impl MaybeTlsStream {
        pub fn peer_addr(&self) -> Result<SocketAddr, tokio::io::Error> {
            match self {
                MaybeTlsStream::Tls(t) => t.as_ref().get_ref().get_ref().0.peer_addr(),
                MaybeTlsStream::Plain(p) => p.as_ref().get_ref().peer_addr(),
            }
        }
    }

    impl AsyncRead for MaybeTlsStream {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, Error>> {
            match self.get_mut() {
                MaybeTlsStream::Tls(t) => t.as_mut().poll_read(cx, buf),
                MaybeTlsStream::Plain(p) => p.as_mut().poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for MaybeTlsStream {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
            match self.get_mut() {
                MaybeTlsStream::Tls(t) => t.as_mut().poll_write(cx, buf),
                MaybeTlsStream::Plain(p) => p.as_mut().poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            match self.get_mut() {
                MaybeTlsStream::Tls(t) => t.as_mut().poll_flush(cx),
                MaybeTlsStream::Plain(p) => p.as_mut().poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            match self.get_mut() {
                MaybeTlsStream::Tls(t) => t.as_mut().poll_shutdown(cx),
                MaybeTlsStream::Plain(p) => p.as_mut().poll_shutdown(cx),
            }
        }
    }

    pub enum MaybeTlsAcceptor<'a, S: Stream<Item = Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, tokio::io::Error>>> {
        Tls(Pin<Box<S>>),
        Plain(Pin<Box<tokio::net::tcp::Incoming<'a>>>),
    }

    impl<'a, S: Stream<Item = Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, tokio::io::Error>>> Stream for MaybeTlsAcceptor<'a, S> {
        type Item = Result<MaybeTlsStream, tokio::io::Error>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.get_mut() {
                MaybeTlsAcceptor::Tls(tls) => tls
                    .as_mut()
                    .poll_next(cx)
                    .map(|t| t.map(|tls_res| tls_res.map(|tls| MaybeTlsStream::Tls(Box::pin(tls))))),
                MaybeTlsAcceptor::Plain(plain) => plain
                    .as_mut()
                    .poll_next(cx)
                    .map(|t| t.map(|tls_res| tls_res.map(|tls| MaybeTlsStream::Plain(Box::pin(tls))))),
            }
        }
    }

    pub fn load_certs(cert_config: &SslConfig) -> Vec<rustls::Certificate> {
        match cert_config {
            SslConfig::FilePath(filename) => {
                let certfile = fs::File::open(filename).expect("cannot open certificate file");
                let mut reader = BufReader::new(certfile);
                rustls::internal::pemfile::certs(&mut reader).expect("Unable to load certificate from file")
            }
            SslConfig::FileData(data) => extract_der_data(data.to_string(), "-----BEGIN CERTIFICATE-----", "-----END CERTIFICATE-----", &|v| {
                rustls::Certificate(v)
            })
            .expect("Unable to load certificate from data"),
        }
    }

    pub fn load_private_key(key_config: &SslConfig) -> rustls::PrivateKey {
        match key_config {
            SslConfig::FilePath(filename) => load_private_key_from_file(&filename),
            SslConfig::FileData(data) => {
                let pkcs8_keys = load_pkcs8_private_key_from_data(data);

                if !pkcs8_keys.is_empty() {
                    pkcs8_keys[0].clone()
                } else {
                    let rsa_keys = load_rsa_private_key_from_data(data);
                    assert!(!rsa_keys.is_empty(), "Unable to load key");
                    rsa_keys[0].clone()
                }
            }
        }
    }

    fn load_private_key_from_file(filename: &str) -> rustls::PrivateKey {
        let rsa_keys = {
            let keyfile = fs::File::open(filename).expect("cannot open private key file");
            let mut reader = BufReader::new(keyfile);
            rustls::internal::pemfile::rsa_private_keys(&mut reader).expect("file contains invalid rsa private key")
        };

        let pkcs8_keys = {
            let keyfile = fs::File::open(filename).expect("cannot open private key file");
            let mut reader = BufReader::new(keyfile);
            rustls::internal::pemfile::pkcs8_private_keys(&mut reader).expect("file contains invalid pkcs8 private key (encrypted keys not supported)")
        };

        // prefer to load pkcs8 keys
        if !pkcs8_keys.is_empty() {
            pkcs8_keys[0].clone()
        } else {
            assert!(!rsa_keys.is_empty(), "Unable to load key");
            rsa_keys[0].clone()
        }
    }

    fn load_pkcs8_private_key_from_data(data: &str) -> Vec<rustls::PrivateKey> {
        extract_der_data(data.to_string(), "-----BEGIN PRIVATE KEY-----", "-----END PRIVATE KEY-----", &|v| {
            rustls::PrivateKey(v)
        })
        .expect("Unable to load private key from data")
    }

    fn load_rsa_private_key_from_data(data: &str) -> Vec<rustls::PrivateKey> {
        extract_der_data(data.to_string(), "-----BEGIN RSA PRIVATE KEY-----", "-----END RSA PRIVATE KEY-----", &|v| {
            rustls::PrivateKey(v)
        })
        .expect("Unable to load private key from data")
    }

    fn extract_der_data<A>(mut data: String, start_mark: &str, end_mark: &str, f: &dyn Fn(Vec<u8>) -> A) -> Result<Vec<A>, ()> {
        let mut ders = Vec::new();

        while let Some(start_index) = data.find(start_mark) {
            let drain_index = start_index + start_mark.len();
            data.drain(..drain_index);
            if let Some(index) = data.find(end_mark) {
                let base64_buf = &data[..index];
                let der = base64::decode(&base64_buf).map_err(|_| ())?;
                ders.push(f(der));

                let drain_index = index + end_mark.len();
                data.drain(..drain_index);
            } else {
                break;
            }
        }

        Ok(ders)
    }
}

/// Inject a http request into saphir
pub async fn inject_raw(req: RawRequest<RawBody>) -> Result<RawResponse<RawBody>, SaphirError> {
    if INIT_STACK.state() != OnceState::Done {
        return Err(SaphirError::Other("Stack is not initialized".to_owned()));
    }

    // # SAFETY #
    // We checked that memory has been initialized above
    let stack = unsafe { STACK.as_ptr().as_ref().expect("Memory has been initialized above.") };

    let saphir_req = Request::new(req.map(Body::from_raw), None);
    REQUEST_FUTURE_COUNT.fetch_add(1, Ordering::SeqCst);
    let saphir_res = stack.invoke(saphir_req).await?;
    Ok(saphir_res.into_raw().map(|r| r.map(|b| b.into_raw()))?)
}
