use spin_sdk::http::{IntoResponse, Params, Request, Response, Router};
use spin_sdk::http_component;
use spin_sdk::key_value::Store;
use spin_sdk::variables;
mod cache;
mod model;
use model::ExplorerAbiResponse;
use serde_json::json;

const ABI_TTL_MS: u64 = 365 * 24 * 60 * 60 * 1000; // 1 year - ABIs are immutable once verified
const ETHERSCAN_BASE_URL: &str = "https://api.etherscan.io/v2/api";

enum AbiLookup {
    Found(serde_json::Value),
    NotFound(AbiMiss),
}

struct AbiMiss {
    provider: &'static str,
    message: String,
    details: String,
}

#[http_component]
fn handle_abi_fetcher(req: Request) -> anyhow::Result<impl IntoResponse> {
    let mut router = Router::new();
    router.get_async("/abi/:chainId/:contractAddress", get_abi);
    Ok(router.handle(req))
}

async fn get_abi(_req: Request, params: Params) -> anyhow::Result<impl IntoResponse> {
    let chain_id = params.get("chainId").unwrap_or("");
    let address = params.get("contractAddress").unwrap_or("");

    if !is_valid_address(address) || !is_valid_chain_id(chain_id) {
        return Ok(build_response(
            400,
            json!({
                "error": "invalid address or chain id",
                "chainId": chain_id,
                "address": address,
            }),
        ));
    }

    let cache_key = format!("abi_{chain_id}_{}", address.to_lowercase());
    let store = Store::open_default().ok();

    if let Some(cached) = store
        .as_ref()
        .and_then(|s| cache::get_cached::<serde_json::Value>(s, &cache_key, ABI_TTL_MS))
    {
        return Ok(build_response(200, cached));
    }

    let etherscan_url = etherscan_abi_url(chain_id, address);
    let etherscan_miss =
        match fetch_explorer_abi("etherscan", etherscan_url, chain_id, address).await {
            Ok(AbiLookup::Found(abi)) => {
                cache_abi(store.as_ref(), &cache_key, &abi);
                return Ok(build_response(200, abi));
            }
            Ok(AbiLookup::NotFound(miss)) => miss,
            Err(resp) => return Ok(response_or_stale(resp, store.as_ref(), &cache_key)),
        };

    if let Some(base_url) = blockscout_base_url(chain_id) {
        let blockscout_url = blockscout_abi_url(base_url, address);
        match fetch_explorer_abi("blockscout", blockscout_url, chain_id, address).await {
            Ok(AbiLookup::Found(abi)) => {
                cache_abi(store.as_ref(), &cache_key, &abi);
                return Ok(build_response(200, abi));
            }
            Ok(AbiLookup::NotFound(blockscout_miss)) => {
                return Ok(build_response(
                    404,
                    json!({
                        "error": "abi not found",
                        "chainId": chain_id,
                        "address": address,
                        "sources": [
                            miss_json(etherscan_miss),
                            miss_json(blockscout_miss),
                        ],
                    }),
                ));
            }
            Err(resp) => return Ok(response_or_stale(resp, store.as_ref(), &cache_key)),
        }
    }

    Ok(build_response(
        404,
        json!({
            "error": etherscan_miss.message,
            "details": etherscan_miss.details,
            "chainId": chain_id,
            "address": address,
            "sources": [miss_json(etherscan_miss)],
        }),
    ))
}

async fn fetch_explorer_abi(
    provider: &'static str,
    url: String,
    chain_id: &str,
    address: &str,
) -> Result<AbiLookup, Response> {
    println!("{} URL: {}", provider, url);

    let resp: Response = match spin_sdk::http::send(Request::get(url)).await {
        Ok(r) => r,
        Err(e) => {
            return Err(build_response(
                502,
                json!({
                    "error": "upstream request failed",
                    "provider": provider,
                    "chainId": chain_id,
                    "address": address,
                    "details": sanitize_err(e),
                }),
            ));
        }
    };

    let api_resp: ExplorerAbiResponse = match serde_json::from_slice(resp.body()) {
        Ok(v) => v,
        Err(e) => {
            return Err(build_response(
                502,
                json!({
                    "error": "invalid upstream json",
                    "provider": provider,
                    "chainId": chain_id,
                    "address": address,
                    "details": sanitize_err(e),
                }),
            ));
        }
    };

    if api_resp.status != "1" {
        return Ok(AbiLookup::NotFound(AbiMiss {
            provider,
            message: api_resp.message,
            details: api_resp.result,
        }));
    }

    let abi_value: serde_json::Value = match serde_json::from_str(&api_resp.result) {
        Ok(v) => v,
        Err(e) => {
            return Err(build_response(
                502,
                json!({
                    "error": "invalid abi json",
                    "provider": provider,
                    "chainId": chain_id,
                    "address": address,
                    "details": sanitize_err(e),
                }),
            ));
        }
    };

    Ok(AbiLookup::Found(abi_value))
}

fn cache_abi(store: Option<&Store>, cache_key: &str, abi: &serde_json::Value) {
    if let Some(s) = store {
        cache::set_cached(s, cache_key, abi);
    }
}

fn response_or_stale(resp: Response, store: Option<&Store>, cache_key: &str) -> Response {
    if let Some(stale) = store.and_then(|s| cache::get_stale::<serde_json::Value>(s, cache_key)) {
        return build_response(200, stale);
    }

    resp
}

fn etherscan_abi_url(chain_id: &str, address: &str) -> String {
    let api_key = variables::get("etherscan_api_key").ok();
    format!(
        "{ETHERSCAN_BASE_URL}?chainId={chain_id}&module=contract&action=getabi&address={address}{}",
        api_key
            .as_ref()
            .map(|k| format!(
                "&apikey={}",
                k.chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
            ))
            .unwrap_or_default()
    )
}

fn blockscout_abi_url(base_url: &str, address: &str) -> String {
    format!("{base_url}/api?module=contract&action=getabi&address={address}")
}

fn blockscout_base_url(chain_id: &str) -> Option<&'static str> {
    match chain_id {
        "1" => Some("https://eth.blockscout.com"),
        "10" => Some("https://optimism.blockscout.com"),
        "100" => Some("https://gnosis.blockscout.com"),
        "42161" => Some("https://arbitrum.blockscout.com"),
        "8453" => Some("https://base.blockscout.com"),
        _ => None,
    }
}

fn miss_json(miss: AbiMiss) -> serde_json::Value {
    json!({
        "provider": miss.provider,
        "error": miss.message,
        "details": miss.details,
    })
}

fn build_response(status: u16, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(body.to_string())
        .build()
}

fn is_valid_address(addr: &str) -> bool {
    let is_prefixed = addr.starts_with("0x") && addr.len() == 42;
    let hex_ok = addr
        .trim_start_matches("0x")
        .chars()
        .all(|c| matches!(c, '0'..='9' | 'a'..='f' | 'A'..='F'));
    is_prefixed && hex_ok
}

fn is_valid_chain_id(chain_id: &str) -> bool {
    chain_id.parse::<u64>().is_ok()
}

fn sanitize_err<E: std::fmt::Display>(e: E) -> String {
    let s = e.to_string();
    s.chars()
        .filter(|c| *c != '"' && *c != '\n' && *c != '\r')
        .collect()
}
