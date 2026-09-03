use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::hex;
use alloy_rlp::Decodable;
use serde_json::{json, Value};
use spin_executor::CancelOnDropToken;
use spin_sdk::http::{IntoResponse, Method, Request, Response, ResponseBuilder};
use spin_sdk::http_component;
use std::future;
use std::task::Poll;
use wasi::clocks::monotonic_clock;

/// Selector of the EEZ registry's `ExecutionNotFound()` error: what a
/// cross-chain call reverts with when run outside a composed sync block.
const EXECUTION_NOT_FOUND: &str = "0xed6bc750";
/// Gas limit reported for cross-chain calls (700000).
const CROSS_CHAIN_GAS: &str = "0xaae60";
/// How long to wait for the front's chain view to catch up with the
/// execution RPC before handing it a cross-chain transaction.
const FRONT_SYNC_MAX_SECS: u64 = 30;

const CORS_HEADERS: [(&str, &str); 4] = [
    ("access-control-allow-origin", "*"),
    ("access-control-allow-methods", "GET, POST, OPTIONS"),
    ("access-control-allow-headers", "*"),
    ("access-control-max-age", "3600"),
];

/// One EEZ chain: an execution RPC for ordinary traffic and a cross-chain
/// front that only accepts transactions touching an EEZ proxy.
struct Chain {
    chain_id: u64,
    execution_rpc: String,
    front: String,
}

/// Parse `EEZ_CHAINS="key=chainId,executionRpc,front;key2=..."`.
fn chains() -> Vec<(String, Chain)> {
    std::env::var("EEZ_CHAINS")
        .unwrap_or_default()
        .split(';')
        .filter_map(|entry| {
            let (key, rest) = entry.trim().split_once('=')?;
            let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
            if parts.len() != 3 {
                return None;
            }
            Some((
                key.trim().to_string(),
                Chain {
                    chain_id: parts[0].parse().ok()?,
                    execution_rpc: parts[1].to_string(),
                    front: parts[2].to_string(),
                },
            ))
        })
        .collect()
}

/// `POST /experimental-eez-rpc/<key>`: a normal JSON-RPC endpoint per chain.
///
/// - `eth_estimateGas` reverting with `ExecutionNotFound()` → fixed 700000.
/// - `eth_sendRawTransaction` whose `eth_call` simulation reverts with
///   `ExecutionNotFound()` → forwarded to the front (once its view has caught
///   up with the execution RPC); everything else → execution RPC verbatim.
#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    if matches!(req.method(), Method::Options) {
        return Ok(with_cors(Response::builder().status(204)).build());
    }

    let key = req
        .path()
        .strip_prefix("/experimental-eez-rpc/")
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let chain = match chains().into_iter().find(|(k, _)| *k == key) {
        Some((_, c)) => c,
        None => {
            let known: Vec<String> = chains().into_iter().map(|(k, _)| k).collect();
            return Ok(json_response(
                404,
                json!({"error": format!("unknown EEZ chain '{key}'; known: {}", known.join(", "))}),
            ));
        }
    };

    if !matches!(req.method(), Method::Post) {
        return Ok(json_response(
            405,
            json!({"error": "method not allowed; POST a JSON-RPC 2.0 request"}),
        ));
    }

    let body = req.body().to_vec();
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return Ok(rpc_error(
                Value::Null,
                -32600,
                &format!("invalid JSON-RPC request: {e}"),
            ))
        }
    };

    let (id, method) = match parsed.as_object() {
        Some(obj) => (
            obj.get("id").cloned().unwrap_or(Value::Null),
            obj.get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        // Batches (and anything else) go to the execution RPC unchanged.
        None => return Ok(forward(&chain.execution_rpc, body).await),
    };

    match method.as_str() {
        // viem's local-account sendTransaction tries this reth/geth extension
        // first; on the execution RPC it fails for cross-chain txs with
        // ExecutionNotFound before anything is signed. Declining it makes
        // clients fall back to eth_estimateGas & co., which we route.
        "eth_fillTransaction" => Ok(rpc_error(id, -32601, "Method not found")),
        "eth_estimateGas" => Ok(estimate_gas(&chain, id, body).await),
        "eth_sendRawTransaction" => Ok(send_raw_transaction(&chain, id, &parsed, body).await),
        _ => Ok(forward(&chain.execution_rpc, body).await),
    }
}

async fn estimate_gas(chain: &Chain, id: Value, body: Vec<u8>) -> Response {
    let upstream = match rpc_post(&chain.execution_rpc, body).await {
        Ok(r) => r,
        Err(e) => return rpc_error(id, -32603, &format!("execution RPC unreachable: {e}")),
    };
    if is_execution_not_found(&upstream) {
        println!(
            "[{}] eth_estimateGas: cross-chain call, answering {CROSS_CHAIN_GAS}",
            chain.chain_id
        );
        return json_response(
            200,
            json!({"jsonrpc": "2.0", "id": id, "result": CROSS_CHAIN_GAS}),
        );
    }
    json_response(200, upstream)
}

async fn send_raw_transaction(chain: &Chain, id: Value, parsed: &Value, body: Vec<u8>) -> Response {
    let raw = parsed["params"][0].as_str().unwrap_or("");
    let call = match decode_call(raw) {
        Some(c) => c,
        None => {
            println!(
                "[{}] eth_sendRawTransaction: undecodable tx, forwarding to execution RPC",
                chain.chain_id
            );
            return forward(&chain.execution_rpc, body).await;
        }
    };

    let sim = json!({"jsonrpc": "2.0", "id": 1, "method": "eth_call", "params": [call, "latest"]});
    let sim_resp = match rpc_json(&chain.execution_rpc, &sim).await {
        Ok(r) => r,
        Err(e) => return rpc_error(id, -32603, &format!("execution RPC unreachable: {e}")),
    };
    if !is_execution_not_found(&sim_resp) {
        println!(
            "[{}] eth_sendRawTransaction: ordinary tx from {}, forwarding to execution RPC",
            chain.chain_id, call["from"]
        );
        return forward(&chain.execution_rpc, body).await;
    }

    println!(
        "[{}] eth_sendRawTransaction: cross-chain tx from {} to {}, routing to front",
        chain.chain_id, call["from"], call["to"]
    );
    if let Err(e) = wait_for_front_sync(chain).await {
        return rpc_error(
            id,
            -32603,
            &format!("front not in sync with execution RPC: {e}"),
        );
    }
    forward(&chain.front, body).await
}

/// Decode a signed transaction (legacy, EIP-2930, EIP-1559, EIP-4844, EIP-7702)
/// into `eth_call` params. `None` when the payload is not a signed tx we know.
fn decode_call(raw: &str) -> Option<Value> {
    let bytes = hex::decode(raw.trim_start_matches("0x")).ok()?;
    let tx = TxEnvelope::decode(&mut bytes.as_slice()).ok()?;
    let from = tx.recover_signer().ok()?;
    Some(json!({
        "from": from.to_string(),
        "to": tx.to().map(|a| a.to_string()),
        "data": hex::encode_prefixed(tx.input()),
        "value": format!("0x{:x}", tx.value()),
    }))
}

/// Wait until the front's block number is at least the execution RPC's; the
/// front keeps its own chain view that can trail by a block.
async fn wait_for_front_sync(chain: &Chain) -> anyhow::Result<()> {
    let target = block_number(&chain.execution_rpc).await?;
    let mut waited = 0;
    loop {
        let front = block_number(&chain.front).await?;
        if front >= target {
            println!(
                "[{}] front at block {front} >= execution {target}",
                chain.chain_id
            );
            return Ok(());
        }
        if waited >= FRONT_SYNC_MAX_SECS {
            anyhow::bail!("front at block {front}, execution RPC at {target} after {waited}s");
        }
        println!(
            "[{}] front at block {front} < execution {target}, waiting",
            chain.chain_id
        );
        sleep_secs(1).await;
        waited += 1;
    }
}

/// Wait `secs` without blocking the component instance.
///
/// `std::thread::sleep` would stall the whole instance instead of yielding to
/// the executor: on Fermyon Cloud a request parked that way holds its slot and
/// its outbound connection for the entire wait, and enough of them exhaust the
/// app's connection allowance — every later request then traps with
/// `ErrorCode::ConnectionLimitReached`, including ones that never route
/// cross-chain.
async fn sleep_secs(secs: u64) {
    let deadline = monotonic_clock::now() + secs * 1_000_000_000;
    // Owned by the closure purely to hold the subscription: assigning a new
    // token drops the previous one, which cancels it, so re-subscribing on
    // every poll cannot pile pollables up in the executor's waker list.
    let mut _token = None;
    future::poll_fn(move |context| {
        if monotonic_clock::now() >= deadline {
            Poll::Ready(())
        } else {
            _token = Some(CancelOnDropToken::from(
                spin_executor::push_waker_and_get_token(
                    monotonic_clock::subscribe_instant(deadline),
                    context.waker().clone(),
                ),
            ));
            Poll::Pending
        }
    })
    .await
}

async fn block_number(url: &str) -> anyhow::Result<u64> {
    let resp = rpc_json(
        url,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []}),
    )
    .await?;
    let hex = resp["result"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no result from {url}: {resp}"))?;
    Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
}

/// Whether a JSON-RPC response carries an `ExecutionNotFound()` revert in
/// `error.data` or `error.data.data`.
fn is_execution_not_found(resp: &Value) -> bool {
    let data = &resp["error"]["data"];
    [data, &data["data"]]
        .iter()
        .filter_map(|d| d.as_str())
        .any(|d| d.to_ascii_lowercase().starts_with(EXECUTION_NOT_FOUND))
}

async fn rpc_json(url: &str, body: &Value) -> anyhow::Result<Value> {
    rpc_post(url, serde_json::to_vec(body)?).await
}

async fn rpc_post(url: &str, body: Vec<u8>) -> anyhow::Result<Value> {
    let resp = send_upstream(url, body).await?;
    Ok(serde_json::from_slice(resp.body())?)
}

async fn send_upstream(url: &str, body: Vec<u8>) -> anyhow::Result<Response> {
    let req = Request::builder()
        .method(Method::Post)
        .uri(url)
        .header("content-type", "application/json")
        .body(body)
        .build();
    let resp: Response = spin_sdk::http::send(req).await?;
    if *resp.status() >= 500 {
        anyhow::bail!("upstream {url} answered HTTP {}", resp.status());
    }
    Ok(resp)
}

/// Forward a request body verbatim and return the upstream body verbatim.
async fn forward(url: &str, body: Vec<u8>) -> Response {
    match send_upstream(url, body).await {
        Ok(resp) => with_cors(
            Response::builder()
                .status(*resp.status())
                .header("content-type", "application/json"),
        )
        .body(resp.body().to_vec())
        .build(),
        Err(e) => rpc_error(Value::Null, -32603, &format!("upstream unreachable: {e}")),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    json_response(
        200,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

fn json_response(status: u16, body: Value) -> Response {
    with_cors(
        Response::builder()
            .status(status)
            .header("content-type", "application/json"),
    )
    .body(body.to_string())
    .build()
}

fn with_cors(builder: &mut ResponseBuilder) -> &mut ResponseBuilder {
    for (k, v) in CORS_HEADERS {
        builder.header(k, v);
    }
    builder
}
