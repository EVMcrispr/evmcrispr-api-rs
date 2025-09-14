use spin_sdk::{
    http::{IntoResponse, Params, Request, Response, Router, Method},
    http_component,
};
use spin_contrib_http::cors::{
    CorsConfig, CorsResponseBuilder, CorsRouter, ALL_HEADERS, ALL_METHODS, ALL_ORIGINS,
};
use spin_contrib_http::request::Contrib;

#[http_component]
async fn handle_hello_world(req: Request) -> Result<impl IntoResponse, anyhow::Error> {
    let cfg = CorsConfig::new(
        ALL_ORIGINS.to_string(),
        ALL_METHODS.to_string(),
        ALL_HEADERS.to_string(),
        false,
        Some(3600),
    );
    let mut router = Router::default();
    router.register_options_handler(&cfg);
    router.any_async("/cors-proxy/*", handler);

    let method = &req.method().clone();
    let request_origin = req.get_header_value_as_string("origin");

    Ok(router
        .handle(req)
        .into_builder()
        .build_with_cors(method, request_origin, &cfg))
}

async fn handler(req: Request, params: Params) -> anyhow::Result<impl IntoResponse> {
    let target = match params.wildcard() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(Response::new(400, "Missing target URL")),
    };

    let (method, headers, body, target) = prepare_proxy_request(&req, target);

    let req = Request::builder()
        .method(method)
        .uri(target)
        .headers(headers)
        .body(body)
        .build();

    // Send the request and await the response
    let res: Response = spin_sdk::http::send(req).await?;
    Ok(res)
}

fn prepare_proxy_request(req: &Request, target: &str) -> (Method, Vec<(String, Vec<u8>)>, Vec<u8>, String) {
    // Preserve method, headers, and body from the incoming request
    let method = req.method().clone();
    let headers: Vec<(String, Vec<u8>)> = req
        .headers()
        .map(|(k, v)| (k.to_owned(), v.as_bytes().to_owned()))
        .collect();
    let body = req.body().to_vec();

    // Reconstruct target with original query string if present
    let mut target = target.to_string();
    let query = req.query();
    if !query.is_empty() {
        let sep = if target.contains('?') { "&" } else { "?" };
        target.push_str(sep);
        target.push_str(query);
    }
    (method, headers, body, target)
}
