# EVMcripsr API

In this repository you can find the source behind the EVMcripsr API:

| URL | Description |
| --- | --- |
| `https://api.evmcrispr.com/cors-proxy/<url>` | CORS Proxy to Giveth GraphQL API |
| `https://api.evmcrispr.com/tokenlist/<chainId>` | Token List API mixing Coingecko and Superfluid |
| `https://api.evmcrispr.com/abi/<chainId>/<contractAddress>` | API to fetch ABIs from Etherscan and other sources |

## Example

```
curl -X POST 'http://localhost:3000/cors-proxy/https://mainnet.serve.giveth.io/graphql' \
    -H 'Content-Type: application/json' \
    --data '{"query":"query GetProjectAddresses($slug: String!) { projectsBySlugs(slugs: [$slug]) { projects { id addresses { address networkId } } } }","variables":{"slug":"evmcrispr-0"}}'
```
