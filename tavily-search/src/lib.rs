use serde_json::json;
use spin_sdk::http::{IntoResponse, Method, Request, Response};
use spin_sdk::http_component;
use spin_sdk::variables;

const TAVILY_URL: &str = "https://api.tavily.com/search";
const DEFAULT_COUNT: u64 = 5;
const MAX_COUNT: u64 = 10;

/// `GET /search?q=<query>&count=<n>` — searches the web via Tavily and returns
/// a trimmed `{query, results: [{title, url, description}]}` payload. The
/// Tavily key stays server-side (Spin variable `tavily_api_key`).
#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    match *req.method() {
        Method::Options => Ok(preflight_response()),
        Method::Get => get_search(req).await,
        _ => Ok(build_response(
            405,
            json!({"error": "method not allowed; use GET /search?q=..."}),
        )),
    }
}

async fn get_search(req: Request) -> anyhow::Result<Response> {
    let query = query_param(req.query(), "q")
        .map(|q| q.trim().to_string())
        .unwrap_or_default();
    if query.is_empty() {
        return Ok(build_response(
            400,
            json!({"error": "missing query. Usage: /search?q=<query>&count=<1-10>"}),
        ));
    }

    let count = query_param(req.query(), "count")
        .and_then(|c| c.parse::<u64>().ok())
        .unwrap_or(DEFAULT_COUNT)
        .clamp(1, MAX_COUNT);

    let api_key = match variables::get("tavily_api_key") {
        Ok(k) => k,
        Err(_) => {
            return Ok(build_response(
                500,
                json!({"error": "search service is not configured"}),
            ));
        }
    };

    let body = json!({
        "query": query,
        "max_results": count,
        "search_depth": "basic",
    })
    .to_string();

    let upstream_req = Request::builder()
        .method(Method::Post)
        .uri(TAVILY_URL)
        .header("authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .body(body)
        .build();

    let resp: Response = match spin_sdk::http::send(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(build_response(
                502,
                json!({"error": "upstream request failed", "details": sanitize_err(e)}),
            ));
        }
    };

    let status = *resp.status();
    // 429 = rate limit, 432 = plan limit exceeded; both mean "back off".
    if status == 429 || status == 432 {
        return Ok(build_response(
            429,
            json!({"error": "search quota or rate limit exceeded"}),
        ));
    }
    if status != 200 {
        return Ok(build_response(
            502,
            json!({
                "error": "upstream search failed",
                "status": status,
                "details": String::from_utf8_lossy(resp.body()).chars().take(300).collect::<String>(),
            }),
        ));
    }

    let parsed: serde_json::Value = match serde_json::from_slice(resp.body()) {
        Ok(v) => v,
        Err(e) => {
            return Ok(build_response(
                502,
                json!({"error": "invalid upstream json", "details": sanitize_err(e)}),
            ));
        }
    };

    let results: Vec<serde_json::Value> = parsed["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    json!({
                        "title": r["title"],
                        "url": r["url"],
                        "description": r["content"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(build_response(
        200,
        json!({"query": query, "results": results}),
    ))
}

/// Extract a raw query-string parameter and percent-decode it.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 3 <= bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn preflight_response() -> Response {
    Response::builder()
        .status(204)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, OPTIONS")
        .header("access-control-allow-headers", "*")
        .header("access-control-max-age", "3600")
        .body(())
        .build()
}

fn build_response(status: u16, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(body.to_string())
        .build()
}

fn sanitize_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
        .chars()
        .filter(|c| *c != '"' && *c != '\n' && *c != '\r')
        .collect()
}
