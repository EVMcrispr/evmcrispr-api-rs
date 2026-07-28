use futures::{SinkExt, StreamExt};
use spin_contrib_http::cors::CorsResponseBuilder;
use spin_contrib_http::cors::{CorsConfig, ALL_HEADERS, ALL_METHODS, ALL_ORIGINS};
use spin_sdk::http::{
    Fields, IncomingRequest, IncomingResponse, Method, OutgoingResponse, Request, ResponseBuilder,
    ResponseOutparam,
};
use spin_sdk::http_component;

/// Upstream bodies are cut off after this many bytes; the chat tool truncates
/// to a much smaller character budget anyway.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// HTTP entrypoint: GET-only proxy for reading arbitrary public https pages.
///
/// Unlike `cors-proxy` there is no target allowlist — safety comes from
/// only allowing GET, only https targets (no scheme downgrade, no internal
/// http services), and never forwarding caller credentials.
#[http_component]
async fn fetch_proxy(req: IncomingRequest, res_out: ResponseOutparam) {
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

    // Preflight requests are between the browser and the proxy: answer them
    // here instead of forwarding OPTIONS upstream.
    if matches!(method_for_req, Method::Options) {
        respond(res_out, 204, "", &method_for_req, origin_for_req).await;
        return;
    }

    if !matches!(method_for_req, Method::Get) {
        respond(
            res_out,
            405,
            "Only GET is supported",
            &method_for_req,
            origin_for_req,
        )
        .await;
        return;
    }

    let target = match get_target(&req) {
        Some(t) => t,
        None => {
            respond(
                res_out,
                400,
                "Malformed or missing target URL. Usage: /fetch/https://<target>",
                &method_for_req,
                origin_for_req,
            )
            .await;
            return;
        }
    };

    println!("Target: {}", target);

    // Never forward the caller's browser-context or credential headers: `host`
    // belongs to the proxy, `origin`/`referer` would trip CORS-aware
    // upstreams, `authorization`/`cookie`/`x-*` could leak Nexus or app
    // credentials to arbitrary sites, and `spin-*` are internal metadata
    // (including the caller's address).
    let forward_headers = headers_for_req
        .into_iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !matches!(
                k.as_str(),
                "host" | "origin" | "referer" | "authorization" | "cookie"
            ) && !k.starts_with("x-")
                && !k.starts_with("spin-")
        })
        .collect::<Vec<(String, Vec<u8>)>>();

    let request = Request::builder()
        .method(Method::Get)
        .uri(&target)
        .headers(forward_headers)
        .build();
    let upstream: IncomingResponse = match spin_sdk::http::send(request).await {
        Ok(r) => r,
        Err(_) => {
            respond(
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

    // Drop upstream CORS headers (ours are appended below; duplicates make
    // browsers reject the response) and never relay upstream cookies. Rewrite
    // redirects back through the proxy so the browser can follow them without
    // leaving our origin.
    let mut headers = upstream
        .headers()
        .entries()
        .into_iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !k.starts_with("access-control-") && k != "set-cookie"
        })
        .map(|(k, v)| {
            if k.eq_ignore_ascii_case("location") {
                let location = String::from_utf8_lossy(&v).into_owned();
                (k, rewrite_location(&target, &location).into_bytes())
            } else {
                (k, v)
            }
        })
        .collect::<Vec<(String, Vec<u8>)>>();

    append_cors_headers(&mut headers, &method_for_req, &origin_for_req);

    if let Err(e) = stream_upstream_to_client(upstream, headers, res_out).await {
        // Streaming already started, so we can't modify the headers
        eprintln!("streaming failed: {e:?}");
    }
}

/// Route a redirect target back through `/fetch/` so the browser follows it as
/// a same-origin request. Relative locations are resolved against the target.
fn rewrite_location(target: &str, location: &str) -> String {
    if location.starts_with("https://") {
        return format!("/fetch/{location}");
    }
    if let Some(rest) = location.strip_prefix('/') {
        if let Some(origin) = https_origin(target) {
            return format!("/fetch/{origin}/{rest}");
        }
    }
    location.to_string()
}

/// `https://host[:port]` part of a URL, if it is https.
fn https_origin(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("https://{host}"))
}

/// Append CORS headers to the provided response header list using
/// `spin_contrib_http::cors::CorsResponseBuilder` with ALLOWED_ORIGINS from environment variable.
fn append_cors_headers(
    headers: &mut Vec<(String, Vec<u8>)>,
    method: &Method,
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
/// into it until completion or the size cap. Returns an error if any stage fails.
async fn stream_upstream_to_client(
    upstream: IncomingResponse,
    headers: Vec<(String, Vec<u8>)>,
    res_out: ResponseOutparam,
) -> anyhow::Result<()> {
    let resp = OutgoingResponse::new(Fields::from_list(&headers)?);
    resp.set_status_code(upstream.status()).unwrap();
    let mut sink = resp.take_body();
    res_out.set(resp);

    let mut sent = 0usize;
    let mut stream = upstream.take_body_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        sent += chunk.len();
        if let Err(e) = sink.send(chunk).await {
            eprintln!("send chunk failed: {e:?}");
            break;
        }
        if sent >= MAX_RESPONSE_BYTES {
            eprintln!("response cut off at {sent} bytes");
            break;
        }
    }
    let _ = sink.flush().await;
    let _ = sink.close().await;
    Ok(())
}

/// Extract the target URL from the incoming request's path, removing the
/// `/fetch/` prefix. Only https targets are accepted.
fn get_target(req: &IncomingRequest) -> Option<String> {
    let path = req.path_with_query()?;
    let target = path.strip_prefix("/fetch/")?.to_string();
    if target.is_empty() || !target.starts_with("https://") {
        return None;
    }
    Some(target)
}

/// Send a non-proxied response (errors, preflight), appending CORS headers.
async fn respond(
    res_out: ResponseOutparam,
    status: u16,
    msg: &str,
    method: &Method,
    origin: String,
) {
    let mut headers: Vec<(String, Vec<u8>)> = Vec::new();
    append_cors_headers(&mut headers, method, &origin);
    let out = OutgoingResponse::new(Fields::from_list(&headers).unwrap());
    out.set_status_code(status).unwrap();
    let _ = res_out.set_with_body(out, msg.as_bytes().to_vec()).await;
}
