//! Minimal `/health` HTTP endpoint for the container HEALTHCHECK.

use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::state::AppState;

pub async fn run(port: u16, state: Arc<AppState>) {
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(error = %e, addr, "failed to bind health endpoint");
            return;
        }
    };
    info!(addr, "health endpoint listening");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "health endpoint accept failed");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let state = state.clone();
                async move { handle(req, state) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                warn!(error = %e, "health connection error");
            }
        });
    }
}

fn handle(
    req: Request<hyper::body::Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.uri().path() != "/health" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from_static(b"not found")))
            .unwrap());
    }

    let (status, body) = if state.is_healthy() {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unhealthy")
    };

    Ok(Response::builder().status(status).body(Full::new(Bytes::from_static(body.as_bytes()))).unwrap())
}
