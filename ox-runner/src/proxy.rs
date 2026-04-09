use anyhow::{Context, Result};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use http_body_util::{BodyExt, Full};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

/// Accumulated metrics from a proxy instance.
#[derive(Debug, Clone, Default)]
pub struct ProxyMetrics {
    pub request_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_latency_ms: u64,
}

/// A running proxy instance.
pub struct ProxyHandle {
    pub local_addr: SocketAddr,
    pub metrics: Arc<Mutex<ProxyMetrics>>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Start a local HTTP proxy that forwards requests to `target_url`,
/// intercepting responses to extract token usage metrics.
///
/// Returns the handle with the local address, shared metrics, and task handle.
pub async fn start_proxy(
    target_url: String,
    provider: String,
) -> Result<ProxyHandle> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding proxy listener")?;
    let local_addr = listener.local_addr()?;
    let metrics = Arc::new(Mutex::new(ProxyMetrics::default()));

    let metrics_clone = Arc::clone(&metrics);
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let target = target_url.clone();
                    let provider = provider.clone();
                    let metrics = Arc::clone(&metrics_clone);

                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req| {
                            let target = target.clone();
                            let provider = provider.clone();
                            let metrics = Arc::clone(&metrics);
                            proxy_request(req, target, provider, metrics)
                        });

                        if let Err(e) = http1::Builder::new()
                            .serve_connection(io, svc)
                            .await
                        {
                            tracing::debug!(err = %e, "proxy connection error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(err = %e, "proxy accept error");
                    break;
                }
            }
        }
    });

    Ok(ProxyHandle {
        local_addr,
        metrics,
        task,
    })
}

async fn proxy_request(
    req: Request<Incoming>,
    target_url: String,
    provider: String,
    metrics: Arc<Mutex<ProxyMetrics>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let start = std::time::Instant::now();

    let method = req.method().clone();
    let path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let url = format!("{target_url}{path}");

    // Collect headers from the incoming request
    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.as_str().to_string(), v.to_string());
        }
    }

    // Read request body
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };

    // Forward request via reqwest
    let client = reqwest::Client::new();
    let mut builder = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST),
        &url,
    );

    for (name, value) in &headers {
        // Skip hop-by-hop headers
        if name == "host" || name == "transfer-encoding" || name == "connection" {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }

    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes);
    }

    match builder.send().await {
        Ok(resp) => {
            let status = resp.status();
            let resp_headers = resp.headers().clone();
            let resp_body = resp.bytes().await.unwrap_or_default();

            let elapsed_ms = start.elapsed().as_millis() as u64;

            // Extract token usage from response body
            extract_metrics(&provider, &resp_body, elapsed_ms, &metrics);

            // Build response
            let mut response = Response::builder().status(status.as_u16());
            for (name, value) in &resp_headers {
                if name == "transfer-encoding" || name == "connection" {
                    continue;
                }
                response = response.header(name, value);
            }

            Ok(response
                .body(Full::new(resp_body))
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))))
        }
        Err(e) => {
            tracing::warn!(err = %e, "proxy upstream error");
            Ok(Response::builder()
                .status(502)
                .body(Full::new(Bytes::from(format!("proxy error: {e}"))))
                .unwrap())
        }
    }
}

fn extract_metrics(
    provider: &str,
    body: &[u8],
    latency_ms: u64,
    metrics: &Arc<Mutex<ProxyMetrics>>,
) {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };

    let (input_tokens, output_tokens) = match provider {
        "anthropic" => {
            let usage = json.get("usage");
            let input = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (input, output)
        }
        "openai" => {
            let usage = json.get("usage");
            let input = usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (input, output)
        }
        _ => (0, 0),
    };

    let mut m = metrics.lock().unwrap();
    m.request_count += 1;
    m.total_input_tokens += input_tokens;
    m.total_output_tokens += output_tokens;
    m.total_latency_ms += latency_ms;
}

/// Collect final metrics from all proxy handles.
pub fn collect_proxy_metrics(handles: &[ProxyHandle]) -> serde_json::Value {
    let mut result = serde_json::Map::new();

    for (i, handle) in handles.iter().enumerate() {
        let m = handle.metrics.lock().unwrap();
        result.insert(
            format!("proxy_{i}"),
            serde_json::json!({
                "request_count": m.request_count,
                "total_input_tokens": m.total_input_tokens,
                "total_output_tokens": m.total_output_tokens,
                "total_latency_ms": m.total_latency_ms,
            }),
        );
    }

    serde_json::Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_anthropic_metrics() {
        let body = serde_json::to_vec(&serde_json::json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
            }
        }))
        .unwrap();

        let metrics = Arc::new(Mutex::new(ProxyMetrics::default()));
        extract_metrics("anthropic", &body, 250, &metrics);

        let m = metrics.lock().unwrap();
        assert_eq!(m.request_count, 1);
        assert_eq!(m.total_input_tokens, 100);
        assert_eq!(m.total_output_tokens, 50);
        assert_eq!(m.total_latency_ms, 250);
    }

    #[test]
    fn extract_openai_metrics() {
        let body = serde_json::to_vec(&serde_json::json!({
            "usage": {
                "prompt_tokens": 200,
                "completion_tokens": 75,
            }
        }))
        .unwrap();

        let metrics = Arc::new(Mutex::new(ProxyMetrics::default()));
        extract_metrics("openai", &body, 500, &metrics);

        let m = metrics.lock().unwrap();
        assert_eq!(m.total_input_tokens, 200);
        assert_eq!(m.total_output_tokens, 75);
    }
}
