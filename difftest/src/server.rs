//! The stub AWS endpoint: a local HTTP/1.1 server that records every
//! request and answers the n-th request of a run with the n-th canned
//! response (the last one repeats).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::response::Resp;

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// Path and query exactly as received.
    pub target: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

struct State {
    responses: Vec<Resp>,
    log: Mutex<Vec<Recorded>>,
}

pub struct Stub {
    pub port: u16,
    state: Arc<State>,
    tasks: Vec<JoinHandle<()>>,
}

impl Stub {
    pub async fn start(responses: Vec<Resp>) -> Result<Stub> {
        let state = Arc::new(State {
            responses,
            log: Mutex::new(vec![]),
        });
        let v4 = TcpListener::bind("127.0.0.1:0").await?;
        let port = v4.local_addr()?.port();
        let mut tasks = vec![tokio::spawn(accept_loop(v4, state.clone()))];
        // `localhost` may resolve to ::1 first; listen there as well.
        if let Ok(v6) =
            TcpListener::bind(SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], port))).await
        {
            tasks.push(tokio::spawn(accept_loop(v6, state.clone())));
        }
        Ok(Stub { port, state, tasks })
    }

    /// The requests received since the last call.
    pub fn take_log(&self) -> Vec<Recorded> {
        std::mem::take(&mut *self.state.log.lock().unwrap())
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn accept_loop(listener: TcpListener, state: Arc<State>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, state.clone()));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    state: Arc<State>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes().to_vec())
        .unwrap_or_default();
    let headers = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect();
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let index = {
        let mut log = state.log.lock().unwrap();
        log.push(Recorded {
            method: parts.method.to_string(),
            target,
            headers,
            body,
        });
        log.len() - 1
    };
    let resp = &state.responses[index.min(state.responses.len() - 1)];
    let mut out = Response::new(Full::new(Bytes::from(resp.body.clone())));
    *out.status_mut() = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
    for (k, v) in &resp.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) else {
            continue;
        };
        // A modeled Content-Type header member comes before the default.
        if name == hyper::header::CONTENT_TYPE && out.headers().contains_key(&name) {
            continue;
        }
        out.headers_mut().append(name, value);
    }
    Ok(out)
}
