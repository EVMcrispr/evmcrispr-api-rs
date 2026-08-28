# EVMcripsr API

In this repository you can find the source behind the EVMcripsr API:

| URL | Description |
| --- | --- |
| `https://api.evmcrispr.com/cors-proxy/<url>` | CORS Proxy to Giveth GraphQL API |
| `https://api.evmcrispr.com/tokenlist/<chainId>` | Token List API mixing Coingecko and Superfluid |
| `https://api.evmcrispr.com/abi/<chainId>/<contractAddress>` | API to fetch ABIs from Etherscan and other sources |
| `https://api.evmcrispr.com/experimental-eez-rpc/<chain>` | One JSON-RPC endpoint per EEZ devnet chain that routes cross-chain transactions to the front |

## Example

```
curl -X POST 'http://localhost:3000/cors-proxy/https://mainnet.serve.giveth.io/graphql' \
    -H 'Content-Type: application/json' \
    --data '{"query":"query GetProjectAddresses($slug: String!) { projectsBySlugs(slugs: [$slug]) { projects { id addresses { address networkId } } } }","variables":{"slug":"evmcrispr-0"}}'
```

## experimental-eez-rpc

The EEZ (Ethereum Economic Zone) devnet exposes, per chain, an execution RPC
that only accepts ordinary transactions and a cross-chain "front" that only
accepts transactions touching an EEZ cross-chain proxy (it holds them and
composes them into a sync block). Neither can estimate gas for a cross-chain
call. `POST /experimental-eez-rpc/<chain>` (`eezL1`, `eezL2`) is one normal
JSON-RPC 2.0 endpoint per chain that routes correctly:

| Request | Behaviour |
| --- | --- |
| any method | forwarded verbatim to the execution RPC (batches too) |
| `eth_estimateGas` | forwarded to the execution RPC; if it reverts with `ExecutionNotFound()` (`0xed6bc750`) the answer is `0xaae60` (700000) |
| `eth_fillTransaction` | always answered with `-32601 Method not found`: viem ≥2.4x tries this reth/geth extension first for local accounts and the execution RPC rejects cross-chain txs with `ExecutionNotFound()` before anything is signed; declining it makes clients fall back to `eth_estimateGas` + `eth_getTransactionCount` + fee queries, which are routed correctly |
| `eth_sendRawTransaction` | the signed tx is decoded (legacy/2930/1559/4844/7702) and simulated with `eth_call` on the execution RPC; if that reverts with `ExecutionNotFound()` the request is forwarded to the front once the front's `eth_blockNumber` has caught up with the execution RPC (polled every second, 30 s max), otherwise to the execution RPC |

Classification rule: a call is cross-chain when the execution RPC reverts it
with the EEZ registry's `ExecutionNotFound()` selector `0xed6bc750` (in
`error.data` or `error.data.data`), which is what a cross-chain proxy reverts
with outside a composed block. Undecodable payloads and unknown chains fall
back to the execution RPC / a 404 respectively; upstream failures come back as
JSON-RPC `-32603` errors, malformed input as `-32600`.

Chains are configured in `spin.toml` through `EEZ_CHAINS`
(`key=chainId,executionRpc,front;...`); every RPC/front host must also be in
the component's `allowed_outbound_hosts`.

```
cast send --rpc-url https://api.evmcrispr.com/experimental-eez-rpc/eezL1 \
    --private-key <key> --gas-limit 700000 \
    0x5e8DEb196c29ca9D828A7120f527482AEA3750F3 'setValue(uint256)' 4242
```
