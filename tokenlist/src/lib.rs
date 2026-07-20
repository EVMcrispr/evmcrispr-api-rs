use chrono::DateTime;
use serde_json::json;
use spin_sdk::http::{IntoResponse, Params, Request, Response, Router};
use spin_sdk::http_component;
use spin_sdk::key_value::Store;

mod cache;
mod model;

use model::{AssetPlatform, SourceTokenList, TokenListResponse, Version};

const TOKEN_LIST_TTL_MS: u64 = 12 * 60 * 60 * 1000; // 12 hours
const PLATFORMS_TTL_MS: u64 = 5 * 24 * 60 * 60 * 1000; // 5 days

#[http_component]
fn handle_tokenlist(req: Request) -> anyhow::Result<impl IntoResponse> {
    let mut router = Router::new();
    router.get_async("/tokenlist/:chainId", get_token_list);
    Ok(router.handle(req))
}

async fn get_token_list(_req: Request, params: Params) -> anyhow::Result<impl IntoResponse> {
    let chain_id_str = params.get("chainId").unwrap_or("");

    let chain_id: u64 = match chain_id_str.parse() {
        Ok(id) if id > 0 => id,
        _ => {
            return Ok(build_response(
                400,
                &json!({
                    "error": "invalid chainId",
                    "chainId": chain_id_str,
                }),
            ));
        }
    };

    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("KV store error: {e:?}");
            return Ok(build_token_list_without_cache(chain_id).await);
        }
    };

    let response_cache_key = format!("tokenlist_{chain_id}");
    if let Some(cached) =
        cache::get_cached::<TokenListResponse>(&store, &response_cache_key, TOKEN_LIST_TTL_MS)
    {
        return Ok(build_response(200, &cached));
    }

    match build_and_cache_token_list(&store, chain_id, &response_cache_key).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            eprintln!("build_and_cache_token_list failed: {e:#}");
            if let Some(stale) = cache::get_stale::<TokenListResponse>(&store, &response_cache_key)
            {
                Ok(build_response(200, &stale))
            } else {
                Ok(build_response(
                    502,
                    &json!({ "error": format!("failed to fetch token lists: {e:#}") }),
                ))
            }
        }
    }
}

async fn build_and_cache_token_list(
    store: &Store,
    chain_id: u64,
    response_cache_key: &str,
) -> anyhow::Result<Response> {
    let (network_name, network_id) = resolve_network(store, chain_id).await?;

    let coingecko_cache_key = format!("coingecko_{network_id}");
    let coingecko_url = format!("https://tokens.coingecko.com/{network_id}/all.json");
    let coingecko_data = fetch_with_cache(store, &coingecko_url, &coingecko_cache_key).await?;

    let last_timestamp = coingecko_data
        .timestamp
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(0);

    let token_list = TokenListResponse {
        name: format!("EVMcrispr Token List ({network_name})"),
        logo_uri: "https://evmcrispr.com/favicon.ico".to_string(),
        timestamp: timestamp_to_iso(last_timestamp),
        tokens: coingecko_data.tokens,
        version: Version {
            major: 1,
            minor: 0,
            patch: 0,
        },
    };

    cache::set_cached(store, response_cache_key, &token_list);

    Ok(build_response(200, &token_list))
}

async fn build_token_list_without_cache(chain_id: u64) -> Response {
    let dummy_store_result = Store::open_default();
    let store_ref = dummy_store_result.as_ref().ok();

    let network = match resolve_network_fetch(chain_id).await {
        Ok(n) => n,
        Err(_) => {
            return build_response(
                400,
                &json!({ "error": format!("unsupported chainId: {chain_id}") }),
            )
        }
    };

    let (network_name, network_id) = network;
    let coingecko_url = format!("https://tokens.coingecko.com/{network_id}/all.json");

    let coingecko_data = fetch_json::<SourceTokenList>(&coingecko_url)
        .await
        .unwrap_or_else(|_| SourceTokenList {
            tokens: vec![],
            timestamp: None,
        });

    let last_timestamp = coingecko_data
        .timestamp
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(0);

    let token_list = TokenListResponse {
        name: format!("EVMcrispr Token List ({network_name})"),
        logo_uri: "https://evmcrispr.com/favicon.ico".to_string(),
        timestamp: timestamp_to_iso(last_timestamp),
        tokens: coingecko_data.tokens,
        version: Version {
            major: 1,
            minor: 0,
            patch: 0,
        },
    };

    if let Some(s) = store_ref {
        cache::set_cached(s, &format!("tokenlist_{chain_id}"), &token_list);
    }

    build_response(200, &token_list)
}

async fn resolve_network(store: &Store, chain_id: u64) -> anyhow::Result<(String, String)> {
    if let Some(platforms) =
        cache::get_cached::<Vec<AssetPlatform>>(store, "platforms", PLATFORMS_TTL_MS)
    {
        if let Some(result) = find_network(&platforms, chain_id) {
            return Ok(result);
        }
    }

    let platforms = fetch_platforms().await?;
    let result = find_network(&platforms, chain_id)
        .ok_or_else(|| anyhow::anyhow!("unsupported chainId: {chain_id}"))?;
    cache::set_cached(store, "platforms", &platforms);
    Ok(result)
}

async fn resolve_network_fetch(chain_id: u64) -> anyhow::Result<(String, String)> {
    let platforms = fetch_platforms().await?;
    find_network(&platforms, chain_id)
        .ok_or_else(|| anyhow::anyhow!("unsupported chainId: {chain_id}"))
}

fn find_network(platforms: &[AssetPlatform], chain_id: u64) -> Option<(String, String)> {
    platforms.iter().find_map(|p| {
        if p.chain_identifier == Some(chain_id as i64) {
            Some((p.name.clone(), p.id.clone()))
        } else {
            None
        }
    })
}

async fn fetch_platforms() -> anyhow::Result<Vec<AssetPlatform>> {
    fetch_json("https://api.coingecko.com/api/v3/asset_platforms").await
}

async fn fetch_with_cache(
    store: &Store,
    url: &str,
    cache_key: &str,
) -> anyhow::Result<SourceTokenList> {
    if let Some(cached) = cache::get_cached::<SourceTokenList>(store, cache_key, TOKEN_LIST_TTL_MS)
    {
        return Ok(cached);
    }

    match fetch_json::<SourceTokenList>(url).await {
        Ok(data) => {
            cache::set_cached(store, cache_key, &data);
            Ok(data)
        }
        Err(e) => {
            if let Some(stale) = cache::get_stale::<SourceTokenList>(store, cache_key) {
                eprintln!("Fetch failed for {url}, using stale cache: {e}");
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

async fn fetch_json<T: serde::de::DeserializeOwned>(url: &str) -> anyhow::Result<T> {
    let req = Request::builder()
        .method(spin_sdk::http::Method::Get)
        .uri(url)
        .header("user-agent", "evmcrispr-api/0.1 (+https://evmcrispr.com)")
        .build();
    let resp: Response = spin_sdk::http::send(req).await?;
    let status = *resp.status();
    if status != 200u16 {
        let body_preview = String::from_utf8_lossy(&resp.body()[..resp.body().len().min(500)]);
        anyhow::bail!("HTTP {status} from {url}: {body_preview}");
    }
    serde_json::from_slice(resp.body())
        .map_err(|e| anyhow::anyhow!("JSON parse error for {url}: {e}"))
}

fn build_response(status: u16, body: &impl serde::Serialize) -> Response {
    let body_str = serde_json::to_string(body)
        .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .header("cache-control", "public, max-age=300, s-maxage=300")
        .body(body_str)
        .build()
}

fn parse_timestamp(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

fn timestamp_to_iso(epoch_secs: i64) -> String {
    DateTime::from_timestamp(epoch_secs, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}
