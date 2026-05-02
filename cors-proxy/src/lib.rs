use futures::{SinkExt, StreamExt};
use spin_contrib_http::cors::CorsResponseBuilder;
use spin_contrib_http::cors::{CorsConfig, ALL_HEADERS, ALL_METHODS, ALL_ORIGINS};
use spin_sdk::http::conversions::TryIntoOutgoingRequest;
use spin_sdk::http::{
    Fields, IncomingRequest, IncomingResponse, OutgoingResponse, Request, ResponseBuilder,
    ResponseOutparam,
};
use spin_sdk::http_component;

/// HTTP entrypoint: acts as a streaming CORS proxy.
///
/// - Extracts the target from the suffix after `/cors-proxy/`.
/// - Streams the incoming request body to the upstream without buffering.
/// - Streams the upstream response back to the client.
/// - Appends CORS headers computed via `spin_contrib_http::cors`.
#[http_component]
async fn cors_proxy(req: IncomingRequest, res_out: ResponseOutparam) {
    let method_for_req = req.method().clone();
    let headers_for_req = req
        .headers()
        .entries()
        .into_iter()
        .collect::<Vec<(String, Vec<u8>)>>();
    let origin_for_req = headers_for_req
        .iter()
        .find(|(k, _)| k == "origin")
        .map(|(_, v)| String::from_utf8(v.to_vec()).unwrap())
        .unwrap_or_default();

    let target = match get_target(&req) {
        Some(t) => t,
        None => {
            handle_error(
                res_out,
                400,
                "Malformed or missing target URL. Usage: /cors-proxy/https://<target>",
                &method_for_req,
                origin_for_req,
            )
            .await;
            return;
        }
    };

    if !is_target_allowed(&target) {
        handle_error(
            res_out,
            403,
            format!(
                "Target domain not allowed. Allowed domains: {}",
                get_allowed_targets().join(", ")
            )
            .as_str(),
            &method_for_req,
            origin_for_req,
        )
        .await;
        return;
    }

    println!("Target: {}", target);

    // Stream the incoming body to the outgoing request and get upstream response
    let upstream: IncomingResponse = match send_upstream_streaming(
        method_for_req.clone(),
        &target,
        headers_for_req,
        req,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            handle_error(
                res_out,
                502,
                "Error sending upstream request",
                &method_for_req,
                origin_for_req,
            )
            .await;
            return;
        }
    };

    // Prepare a streaming response to the client
    let mut headers = upstream
        .headers()
        .entries()
        .into_iter()
        .collect::<Vec<(String, Vec<u8>)>>();

    // Add your CORS headers here if needed (use request Origin)
    append_cors_headers(&mut headers, &method_for_req, &origin_for_req);

    if let Err(e) = stream_upstream_to_client(upstream, headers, res_out).await {
        // Streaming already started, so we can't modify the headers
        eprintln!("streaming failed: {e:?}");
    }
}

/// Build and send an outgoing request while streaming the incoming body.
///
/// - Creates an `OutgoingRequest` with method, target, and headers.
/// - Pipes chunks from `IncomingRequest::into_body_stream()` to the
///   request body's sink.
/// - Flushes/closes the request body and awaits the upstream `IncomingResponse`.
async fn send_upstream_streaming(
    method: spin_sdk::http::Method,
    target: &str,
    headers: Vec<(String, Vec<u8>)>,
    req: IncomingRequest,
) -> anyhow::Result<IncomingResponse> {
    let (outgoing_req, _) = Request::builder()
        .method(method)
        .uri(target)
        .headers(headers)
        .build()
        .try_into_outgoing_request()?;
    let mut body_sink = outgoing_req.take_body();
    let send_fut = spin_sdk::http::send(outgoing_req);

    let mut in_stream = req.into_body_stream();
    while let Some(chunk) = in_stream.next().await {
        body_sink.send(chunk?).await?;
    }
    body_sink.flush().await?;
    body_sink.close().await?;

    Ok(send_fut.await?)
}

/// Append CORS headers to the provided response header list using
/// `spin_contrib_http::cors::CorsResponseBuilder` with ALLOWED_ORIGINS from environment variable.
fn append_cors_headers(
    headers: &mut Vec<(String, Vec<u8>)>,
    method: &spin_sdk::http::Method,
    origin: &str,
) {
    let allowed_origins =
        std::env::var("ALLOWED_ORIGINS").unwrap_or_else(|_| ALL_ORIGINS.to_string());
    let cfg = CorsConfig::new(
        allowed_origins,
        ALL_METHODS.to_string(),
        ALL_HEADERS.to_string(),
        false,
        Some(3600),
    );
    let mut rb = ResponseBuilder::new(200u16);
    let cors_resp = rb.build_with_cors(method, origin.to_string(), &cfg);
    for (k, v) in cors_resp.headers() {
        headers.push((k.to_string(), v.as_bytes().to_vec()));
    }
}

/// Start the client response immediately, then stream upstream body chunks
/// into it until completion. Returns an error if any stage fails.
async fn stream_upstream_to_client(
    upstream: IncomingResponse,
    headers: Vec<(String, Vec<u8>)>,
    res_out: ResponseOutparam,
) -> anyhow::Result<()> {
    let resp = OutgoingResponse::new(Fields::from_list(&headers)?);
    resp.set_status_code(upstream.status()).unwrap();
    let mut sink = resp.take_body();
    res_out.set(resp);

    let mut stream = upstream.take_body_stream();
    while let Some(chunk) = stream.next().await {
        if let Err(e) = sink.send(chunk?).await {
            eprintln!("send chunk failed: {e:?}");
            break;
        }
    }
    let _ = sink.flush().await;
    let _ = sink.close().await;
    Ok(())
}

/// Extract the target URL from the incoming request's path, removing the
/// `/cors-proxy/` prefix. Returns `None` if missing or malformed.
fn get_target(req: &IncomingRequest) -> Option<String> {
    let path = req.path_with_query()?;
    let target = path.strip_prefix("/cors-proxy/")?.to_string();
    if target.is_empty() {
        return None;
    }
    // Check if target is a valid URL (must start with http:// or https://)
    if !(target.starts_with("http://") || target.starts_with("https://")) {
        return None;
    }
    Some(target)
}

// Get allowed targets from environment variable
fn get_allowed_targets() -> Vec<String> {
    std::env::var("ALLOWED_TARGET_DOMAINS")
        .unwrap_or_default()
        .split(",")
        .map(|s| s.trim().to_string())
        .collect::<Vec<String>>()
}

/// Check if the target is allowed (prefix match against the allowlist)
fn is_target_allowed(target: &str) -> bool {
    let allowed_targets = get_allowed_targets();
    allowed_targets.is_empty() || allowed_targets.iter().any(|a| target.starts_with(a))
}

/// Handle error (appending CORS headers)
async fn handle_error(
    res_out: ResponseOutparam,
    status: u16,
    msg: &str,
    method: &spin_sdk::http::Method,
    origin: String,
) {
    let mut headers: Vec<(String, Vec<u8>)> = Vec::new();
    append_cors_headers(&mut headers, method, &origin);
    let out = OutgoingResponse::new(Fields::from_list(&headers).unwrap());
    out.set_status_code(status).unwrap();
    let _ = res_out.set_with_body(out, msg.as_bytes().to_vec()).await;
}
