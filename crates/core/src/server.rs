//! Server module
use std::future::Future;
use std::io::Result as IoResult;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[cfg(feature = "http1")]
use hyper::server::conn::http1;
#[cfg(feature = "http2")]
use hyper::server::conn::http2;
use tokio::sync::Notify;
use tokio::time::Duration;

use crate::conn::{Accepted, Acceptor, HttpBuilders, IntoAcceptor, LocalAddr};
use crate::http::HttpConnection;
use crate::runtimes::TokioExecutor;
use crate::Service;

/// HTTP Server
///
/// A `Server` is created to listen on a port, parse HTTP requests, and hand them off to a [`Service`].
pub struct Server<A> {
    acceptor: A,
    builders: Arc<HttpBuilders>,
}

impl<A: Acceptor> Server<A> {
    /// Create new `Server` with [`Listener`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use salvo_core::prelude::*;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// Server::new(TcpListener::bind("127.0.0.1:7878")).await;
    /// # }
    /// ```
    #[inline]
    pub async fn new<T>(acceptor: T) -> Self
    where
        T: IntoAcceptor<Acceptor = A>,
    {
        Self::try_new(acceptor).await.unwrap()
    }
    /// Create new `Server` with [`Listener`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use salvo_core::prelude::*;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// Server::try_new(TcpListener::bind("127.0.0.1:7878")).await.unwrap();
    /// # }
    /// ```
    #[inline]
    pub async fn try_new<T>(acceptor: T) -> IoResult<Self>
    where
        T: IntoAcceptor<Acceptor = A>,
    {
        Ok(Server {
            acceptor: acceptor.into_acceptor().await?,
            builders: HttpBuilders {
                #[cfg(feature = "http1")]
                http1: http1::Builder::new(),
                #[cfg(feature = "http2")]
                http2: http2::Builder::new(TokioExecutor),
                #[cfg(feature = "http3")]
                http3: crate::conn::http3::Http3Builder,
            }
            .into(),
        })
    }

    #[inline]
    pub fn local_addrs(&self) -> Vec<LocalAddr> {
        self.acceptor.local_addrs()
    }

    // Use this function to set http protocol.
    // pub fn http1_mut(&mut self) -> &mut http1::Builder {
    //     &mut self.http1
    // }

    // /// Use this function to set http protocol.
    // pub fn http2_mut(&mut self) -> &mut http2::Builder {
    //     &mut self.http2
    // }

    /// Serve a [`Service`]
    #[inline]
    pub async fn serve<S>(self, service: S)
    where
        S: Into<Service>,
    {
        self.try_serve(service).await.unwrap();
    }

    /// Try to serve a [`Service`]
    #[inline]
    pub async fn try_serve<S>(self, service: S) -> IoResult<()>
    where
        S: Into<Service>,
    {
        self.try_serve_with_graceful_shutdown(service, futures_util::future::pending(), None)
            .await
    }

    /// Serve with graceful shutdown signal.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use tokio::sync::oneshot;
    ///
    /// use salvo_core::prelude::*;
    ///
    /// #[handler]
    /// async fn hello(res: &mut Response) {
    ///     res.render("Hello World!");
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let (tx, rx) = oneshot::channel();
    ///     let router = Router::new().get(hello);
    ///     let server = Server::new(TcpListener::bind("127.0.0.1:7878")).await.serve_with_graceful_shutdown(router, async {
    ///         rx.await.ok();
    ///     }, None);
    ///
    ///     // Spawn the server into a runtime
    ///     tokio::task::spawn(server);
    ///
    ///     // Later, start the shutdown...
    ///     let _ = tx.send(());
    /// }
    /// ```
    #[inline]
    pub async fn serve_with_graceful_shutdown<S, G>(self, service: S, signal: G, timeout: Option<Duration>)
    where
        S: Into<Service>,
        G: Future<Output = ()> + Send + 'static,
    {
        self.try_serve_with_graceful_shutdown(service, signal, timeout)
            .await
            .unwrap();
    }

    /// Serve with graceful shutdown signal.
    #[inline]
    pub async fn try_serve_with_graceful_shutdown<S, G>(
        mut self,
        service: S,
        signal: G,
        timeout: Option<Duration>,
    ) -> IoResult<()>
    where
        S: Into<Service>,
        G: Future<Output = ()> + Send + 'static,
    {
        let alive_connections = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());
        let timeout_notify = Arc::new(Notify::new());

        tokio::pin!(signal);

        for addr in self.acceptor.local_addrs() {
            tracing::info!("listening on {}", addr);
        }

        let service = Arc::new(service.into());
        loop {
            let builders = self.builders.clone();
            tokio::select! {
                _ = &mut signal => {
                    if let Some(timeout) = timeout {
                        tracing::info!(
                            timeout_in_seconds = timeout.as_secs_f32(),
                            "initiate graceful shutdown",
                        );

                        let timeout_notify = timeout_notify.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(timeout).await;
                            timeout_notify.notify_waiters();
                        });
                    } else {
                        tracing::info!("initiate graceful shutdown");
                    }
                    break;
                },
                 accepted = self.acceptor.accept() => {
                    match accepted {
                        Ok(Accepted { conn, local_addr, remote_addr }) => {
                            let service = service.clone();
                            let alive_connections = alive_connections.clone();
                            let notify = notify.clone();
                            let timeout_notify = timeout_notify.clone();
                            let handler = service.hyper_handler(local_addr, remote_addr);
                            tokio::spawn(async move {
                                alive_connections.fetch_add(1, Ordering::SeqCst);
                                let conn = conn.serve(handler, builders);
                                if timeout.is_some() {
                                    tokio::select! {
                                        result = conn => {
                                            if let Err(e) = result {
                                                tracing::error!(error = ?e, "http1 serve connection failed");
                                            }
                                        },
                                        _ = timeout_notify.notified() => {}
                                    }
                                } else if let Err(e) = conn.await {
                                    tracing::error!(error = ?e, "http1 serve connection failed");
                                }

                                if alive_connections.fetch_sub(1, Ordering::SeqCst) == 1 {
                                    notify.notify_one();
                                }
                            });
                        },
                        Err(e) => {
                            tracing::error!(error = ?e, "accept connection failed");
                        }
                    }
                }
            }
        }

        if alive_connections.load(Ordering::SeqCst) > 0 {
            tracing::info!("wait for all connections to close.");
            notify.notified().await;
        }

        tracing::info!("server stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use crate::conn::{Acceptor, IntoAcceptor};
    use crate::prelude::*;

    #[tokio::test]
    async fn test_server() {
        #[handler(internal)]
        async fn hello() -> Result<&'static str, ()> {
            Ok("Hello World")
        }
        #[handler(internal)]
        async fn json(res: &mut Response) {
            #[derive(Serialize, Debug)]
            struct User {
                name: String,
            }
            res.render(Json(User { name: "jobs".into() }));
        }
        let router = Router::new().get(hello).push(Router::with_path("json").get(json));
        let listener = TcpListener::bind("127.0.0.1:0");
        let server = Server::new(listener).await;
        let addr = server.local_addrs().remove(0);
        let addr = addr.into_std().unwrap();
        tokio::spawn(async move {
            server.serve(router).await;
        });

        let base_url = format!("http://{}", addr);
        let client = reqwest::Client::new();
        let result = client.get(&base_url).send().await.unwrap().text().await.unwrap();
        assert_eq!(result, "Hello World");

        let client = reqwest::Client::new();
        let result = client
            .get(format!("{}/json", base_url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(result, r#"{"name":"jobs"}"#);

        let result = client
            .get(format!("{}/not_exist", base_url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(result.contains("Not Found"));
        let result = client
            .get(format!("{}/not_exist", base_url))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(result.contains(r#""code":404"#));
        let result = client
            .get(format!("{}/not_exist", base_url))
            .header("accept", "text/plain")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(result.contains("code:404"));
        let result = client
            .get(format!("{}/not_exist", base_url))
            .header("accept", "application/xml")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(result.contains("<code>404</code>"));
    }
}
