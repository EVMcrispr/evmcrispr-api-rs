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

/// Selectors of the EEZ registry errors a cross-chain call reverts with when
/// run outside a composed sync block: `ExecutionNotFound()` (no entry for
/// the call), and the block gate that fires before the lookup on either
/// side — L1's `ExecutionNotInCurrentBlock(uint64 rollupId)` and L2's
/// `ExecutionNotInCurrentBlock()`.
const CROSS_CHAIN_REVERTS: [&str; 3] = ["0xed6bc750", "0x9a499f3b", "0xf9d330ad"];
/// Gas limit reported for cross-chain calls (700000).
const CROSS_CHAIN_GAS: &str = "0xaae60";
/// How long to wait for the front's chain view to catch up with the
/// execution RPC before handing it a cross-chain transaction.
const FRONT_SYNC_MAX_SECS: u64 = 30;

/// Errors that contracts raise around a call they made that reverted, keeping
/// only the target and the calldata: the inner reason is dropped, so a
/// cross-chain call wrapped in one of these no longer carries
/// `ExecutionNotFound()`. Replaying the wrapped call is the only way to tell.
/// `(selector, address word, bytes-offset word)`, indexed into the arguments.
const CALL_WRAPPERS: [(&str, usize, usize); 3] = [
    // Assertions `ERC8211.CallFailed(address target, bytes data)`
    ("0x6c544f33", 0, 1),
    // Assertions `Operators.RawCallFailed(address target, bytes data)`
    ("0xf8ec3958", 0, 1),
    // Assertions `Operators.LambdaCallFailed(uint256 index, address target, bytes callData)`
    ("0x56573122", 1, 2),
];
/// How many wrappers to peel before giving up: each costs one `eth_call`.
const MAX_UNWRAP_DEPTH: u8 = 3;

/// Safe `execTransaction(address to, uint256 value, bytes data, uint8
/// operation, …)`. A Safe reverts a failed inner call with a bare `GS013`,
/// dropping its reason, so a cross-chain call made through a Safe is only
/// recognisable by replaying the inner call from the Safe itself.
const SAFE_EXEC_TRANSACTION: &str = "0x6a761202";
/// `multiSend(bytes transactions)` of Safe's MultiSend / MultiSendCallOnly:
/// what a Safe delegatecalls to make several calls in one transaction.
const MULTI_SEND: &str = "0x8d80ff0a";

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
///
/// Either revert also counts when it arrives wrapped in a caller's own error
/// (see `CALL_WRAPPERS`), or hidden behind a Safe's `execTransaction` (see
/// `SAFE_EXEC_TRANSACTION`): the wrapped call is replayed to find out.
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
        "eth_estimateGas" => Ok(estimate_gas(&chain, id, &parsed, body).await),
        "eth_sendRawTransaction" => Ok(send_raw_transaction(&chain, id, &parsed, body).await),
        _ => Ok(forward(&chain.execution_rpc, body).await),
    }
}

async fn estimate_gas(chain: &Chain, id: Value, parsed: &Value, body: Vec<u8>) -> Response {
    let call = &parsed["params"][0];
    let upstream = match rpc_post(&chain.execution_rpc, body).await {
        Ok(r) => r,
        Err(e) => return rpc_error(id, -32603, &format!("execution RPC unreachable: {e}")),
    };
    if is_cross_chain(chain, call, &upstream, 0).await {
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
    if !is_cross_chain(chain, &call, &sim_resp, 0).await {
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

/// Whether lowercase revert `data` is one of the registry errors that mean
/// "this call only resolves inside a composed sync block".
fn is_cross_chain_revert(data: &str) -> bool {
    CROSS_CHAIN_REVERTS.iter().any(|s| data.starts_with(s))
}

/// Revert data carried by a JSON-RPC error, in `error.data` or
/// `error.data.data` depending on the node.
fn revert_data(resp: &Value) -> Option<String> {
    let data = &resp["error"]["data"];
    [data, &data["data"]]
        .iter()
        .find_map(|d| d.as_str())
        .map(str::to_ascii_lowercase)
}

/// Whether `call` (an `eth_call` params object) reverted with `resp` because
/// it is cross-chain: one of `CROSS_CHAIN_REVERTS` itself, a wrapper around a
/// call that reverts with one, or a Safe `execTransaction` whose inner call does.
/// A wrapper keeps only the target and the calldata, and a Safe keeps
/// nothing at all, so the inner call is replayed against the execution RPC
/// to see what it really reverts with — up to `MAX_UNWRAP_DEPTH` layers,
/// since wrappers nest.
async fn is_cross_chain(chain: &Chain, call: &Value, resp: &Value, depth: u8) -> bool {
    let Some(data) = revert_data(resp) else {
        return false;
    };
    if is_cross_chain_revert(&data) {
        return true;
    }
    if depth >= MAX_UNWRAP_DEPTH {
        println!(
            "[{}] stopped unwrapping after {depth} wrappers",
            chain.chain_id
        );
        return false;
    }
    let from = call["from"].as_str().unwrap_or_default();
    let inner_calls = if let Some((target, inner)) = unwrap_call(&data) {
        println!(
            "[{}] revert wraps a call to {target}, replaying it to see why it failed",
            chain.chain_id
        );
        vec![json!({"from": from, "to": target, "data": inner})]
    } else {
        let calls = safe_inner_calls(call);
        if !calls.is_empty() {
            println!(
                "[{}] revert comes from a Safe transaction, replaying its {} inner call(s) from the Safe",
                chain.chain_id,
                calls.len()
            );
        }
        calls
    };
    for inner in inner_calls {
        let probe = json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_call",
            "params": [inner, "latest"],
        });
        match rpc_json(&chain.execution_rpc, &probe).await {
            // Boxed because the recursion makes this future's size unbounded.
            Ok(inner_resp) => {
                if Box::pin(is_cross_chain(chain, &inner, &inner_resp, depth + 1)).await {
                    return true;
                }
            }
            Err(e) => {
                println!("[{}] replay of {} failed: {e}", chain.chain_id, inner["to"]);
            }
        }
    }
    false
}

/// The calls a Safe makes for an `execTransaction` calldata, as `eth_call`
/// params from the Safe's own address: the inner call itself, or — when it
/// delegatecalls a MultiSend — each packed call. Empty when `call` is not a
/// Safe transaction we can replay (a delegatecall to anything else has no
/// stand-alone equivalent).
fn safe_inner_calls(call: &Value) -> Vec<Value> {
    let (Some(safe), Some(data)) = (call["to"].as_str(), call["data"].as_str()) else {
        return vec![];
    };
    let Some((to, value, inner, operation)) = decode_exec_transaction(data) else {
        return vec![];
    };
    let as_call = |to: &str, value: &str, data: &str| json!({"from": safe, "to": to, "data": data, "value": value});
    match operation {
        0 => vec![as_call(&to, &value, &inner)],
        1 => decode_multi_send(&inner)
            .into_iter()
            .filter(|(op, _, _, _)| *op == 0)
            .map(|(_, to, value, data)| as_call(&to, &value, &data))
            .collect(),
        _ => vec![],
    }
}

/// `(to, value, data, operation)` of a Safe `execTransaction` calldata.
fn decode_exec_transaction(data: &str) -> Option<(String, String, String, u8)> {
    let bytes = hex::decode(data.trim_start_matches("0x")).ok()?;
    if hex::encode_prefixed(bytes.get(..4)?) != SAFE_EXEC_TRANSACTION {
        return None;
    }
    let args = &bytes[4..];
    let word = |i: usize| args.get(i * 32..(i + 1) * 32);
    let to = hex::encode_prefixed(&word(0)?[12..]);
    let value = hex_quantity(word(1)?);
    let offset = low_u32(word(2)?)?;
    let len = low_u32(args.get(offset..offset + 32)?)?;
    let inner = hex::encode_prefixed(args.get(offset + 32..offset + 32 + len)?);
    let operation = word(3)?[31];
    Some((to, value, inner, operation))
}

/// The `(operation, to, value, data)` entries packed into a `multiSend(bytes)`
/// calldata: each is `operation(1) ‖ to(20) ‖ value(32) ‖ length(32) ‖ data`.
/// Empty when the calldata is not a MultiSend call or is malformed.
fn decode_multi_send(data: &str) -> Vec<(u8, String, String, String)> {
    let Ok(bytes) = hex::decode(data.trim_start_matches("0x")) else {
        return vec![];
    };
    if bytes.get(..4).map(hex::encode_prefixed).as_deref() != Some(MULTI_SEND) {
        return vec![];
    }
    let args = &bytes[4..];
    let Some(packed) = low_u32(&args[..32.min(args.len())])
        .and_then(|offset| low_u32(args.get(offset..offset + 32)?).map(|len| (offset, len)))
        .and_then(|(offset, len)| args.get(offset + 32..offset + 32 + len))
    else {
        return vec![];
    };
    let mut entries = vec![];
    let mut at = 0;
    while at + 85 <= packed.len() {
        let operation = packed[at];
        let to = hex::encode_prefixed(&packed[at + 1..at + 21]);
        let value = hex_quantity(&packed[at + 21..at + 53]);
        let Some(len) = low_u32(&packed[at + 53..at + 85]) else {
            break;
        };
        let Some(call_data) = packed.get(at + 85..at + 85 + len) else {
            break;
        };
        entries.push((operation, to, value, hex::encode_prefixed(call_data)));
        at += 85 + len;
    }
    entries
}

/// A 32-byte word as a JSON-RPC quantity (`0x0`, no leading zeros).
fn hex_quantity(word: &[u8]) -> String {
    let trimmed = hex::encode(word).trim_start_matches('0').to_string();
    format!("0x{}", if trimmed.is_empty() { "0" } else { &trimmed })
}

/// The low 4 bytes of a word: the only part of an offset or length that can
/// be meaningful here, the whole payload being far smaller than 4GiB.
fn low_u32(word: &[u8]) -> Option<usize> {
    if word.len() != 32 {
        return None;
    }
    Some(u32::from_be_bytes(word[28..32].try_into().ok()?) as usize)
}

/// Pull `(target, calldata)` out of one of the `CALL_WRAPPERS` errors.
/// `None` when the data is not a wrapper we know or is malformed.
fn unwrap_call(data: &str) -> Option<(String, String)> {
    let bytes = hex::decode(data.trim_start_matches("0x")).ok()?;
    let selector = hex::encode_prefixed(bytes.get(..4)?);
    let (_, address_word, bytes_word) = CALL_WRAPPERS.iter().find(|(s, _, _)| *s == selector)?;
    let args = &bytes[4..];
    let word = |i: usize| args.get(i * 32..(i + 1) * 32);
    let target = hex::encode_prefixed(&word(*address_word)?[12..]);
    let offset = low_u32(word(*bytes_word)?)?;
    let len = low_u32(args.get(offset..offset + 32)?)?;
    let calldata = args.get(offset + 32..offset + 32 + len)?;
    Some((target, hex::encode_prefixed(calldata)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn word(hex_str: &str) -> String {
        format!("{:0>64}", hex_str.trim_start_matches("0x"))
    }

    /// `execTransaction(to, value, data, operation, 0, 0, 0, 0x0, 0x0, sig)`
    /// with `data` and `signatures` as dynamic tails.
    fn exec_transaction(to: &str, value: u64, data: &str, operation: u8) -> String {
        let data_hex = data.trim_start_matches("0x");
        let mut s = String::from("6a761202");
        s += &word(to);
        s += &word(&format!("{value:x}"));
        s += &word("140"); // data offset: 10 words
        s += &word(&format!("{operation:x}"));
        for _ in 0..5 {
            s += &word("0");
        }
        s += &word("180"); // signatures offset: after data (len word + 1 word)
        s += &word(&format!("{:x}", data_hex.len() / 2));
        s += &format!("{data_hex:0<64}");
        s += &word("0"); // empty signatures
        format!("0x{s}")
    }

    #[test]
    fn decodes_a_plain_exec_transaction() {
        let data = exec_transaction(
            "0xa55e472841ca3d318205036724a94f5abdbf7b18",
            7,
            "0x0ce105e2",
            0,
        );
        let (to, value, inner, operation) = decode_exec_transaction(&data).unwrap();
        assert_eq!(to, "0xa55e472841ca3d318205036724a94f5abdbf7b18");
        assert_eq!(value, "0x7");
        assert_eq!(inner, "0x0ce105e2");
        assert_eq!(operation, 0);
    }

    #[test]
    fn recognises_every_registry_gate_as_cross_chain() {
        // ExecutionNotFound()
        assert!(is_cross_chain_revert("0xed6bc750"));
        // L1 ExecutionNotInCurrentBlock(uint64 rollupId)
        assert!(is_cross_chain_revert(&format!("0x9a499f3b{}", word("1"))));
        // L2 ExecutionNotInCurrentBlock()
        assert!(is_cross_chain_revert("0xf9d330ad"));
        // Something else that reverted for its own reasons
        assert!(!is_cross_chain_revert("0x08c379a0"));
    }

    #[test]
    fn ignores_other_selectors() {
        assert!(decode_exec_transaction("0xdeadbeef").is_none());
        assert!(decode_multi_send("0xdeadbeef").is_empty());
    }

    #[test]
    fn decodes_packed_multi_send_entries() {
        // Two entries: a call with 4 bytes of data, then a delegatecall with none.
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let packed = format!(
            "00{a}{}{}0ce105e201{b}{}{}",
            word("5"),
            word("4"),
            word("0"),
            word("0")
        );
        let data = format!(
            "0x8d80ff0a{}{}{packed:0<128}",
            word("20"),
            word(&format!("{:x}", packed.len() / 2))
        );
        let entries = decode_multi_send(&data);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            (0, format!("0x{a}"), "0x5".into(), "0x0ce105e2".into())
        );
        assert_eq!(entries[1], (1, format!("0x{b}"), "0x0".into(), "0x".into()));
    }

    #[test]
    fn replays_only_calls_from_the_safe() {
        let a = "1111111111111111111111111111111111111111";
        let packed = format!("00{a}{}{}0ce105e2", word("0"), word("4"));
        let multi = format!(
            "0x8d80ff0a{}{}{packed:0<128}",
            word("20"),
            word(&format!("{:x}", packed.len() / 2))
        );
        let call = json!({
            "from": "0xowner",
            "to": "0x5afe",
            "data": exec_transaction("0x7b21bbdbde8d01df591fdc2dc0be9956dde1e16c", 0, &multi, 1),
        });
        let inner = safe_inner_calls(&call);
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["from"], "0x5afe");
        assert_eq!(inner[0]["to"], format!("0x{a}"));
        assert_eq!(inner[0]["data"], "0x0ce105e2");
        // A delegatecall to something that is not a MultiSend has no replay.
        let call = json!({"from": "0xowner", "to": "0x5afe", "data": exec_transaction("0xabcd", 0, "0x", 1)});
        assert!(safe_inner_calls(&call).is_empty());
    }
}
