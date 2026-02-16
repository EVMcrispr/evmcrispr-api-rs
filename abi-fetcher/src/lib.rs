use spin_sdk::http::{IntoResponse, Params, Request, Response, Router};
use spin_sdk::http_component;
use spin_sdk::variables;
mod model;
use model::EtherscanAbiResponse;
use serde_json::json;

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
        return Ok(build_response(400, json!({
            "error": "invalid address or chain id",
            "chainId": chain_id,
            "address": address,
        })));
    }

    let base_url = "https://api.etherscan.io/v2/api";
    let api_key = variables::get("etherscan_api_key").ok();
    // let api_key = std::env::var("ETHERSCAN_API_KEY").ok();
    let url = format!(
        "{base_url}?chainId={chain_id}&module=contract&action=getabi&address={address}{}",
        api_key
            .as_ref()
            .map(|k| 
                format!("&apikey={}", k.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>())
            )
            .unwrap_or_default()
    );
    println!("URL: {}", url);

    let resp: Response = match spin_sdk::http::send(Request::get(url)).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(build_response(502, json!({
                "error": "upstream request failed",
                "details": sanitize_err(e),
            })));
        }
    };

    let api_resp: EtherscanAbiResponse = match serde_json::from_slice(resp.body()) {
        Ok(v) => v,
        Err(e) => {
            return Ok(build_response(502, json!({
                "error": "invalid upstream json",
                "details": sanitize_err(e),
            })));
        }
    };

    if api_resp.status != "1" {
        return Ok(build_response(404, json!({
            "error": api_resp.message,
            "details": api_resp.result,
            "chainId": chain_id,
            "address": address,
        })));
    }

    let abi_value: serde_json::Value = match serde_json::from_str(&api_resp.result) {
        Ok(v) => v,
        Err(e) => {
            return Ok(build_response(502, json!({
                "error": "invalid abi json",
                "details": sanitize_err(e),
            })));
        }
    };

    Ok(build_response(200, abi_value))
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
    s.chars().filter(|c| *c != '"' && *c != '\n' && *c != '\r').collect()
}