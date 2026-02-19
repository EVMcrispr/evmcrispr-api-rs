use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct AssetPlatform {
    pub id: String,
    pub chain_identifier: Option<i64>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(default)]
    pub address: String,
    #[serde(rename = "chainId", default)]
    pub chain_id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub decimals: u32,
    #[serde(rename = "logoURI", skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceTokenList {
    #[serde(default)]
    pub tokens: Vec<Token>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenListResponse {
    pub name: String,
    #[serde(rename = "logoURI")]
    pub logo_uri: String,
    pub timestamp: String,
    pub tokens: Vec<Token>,
    pub version: Version,
}
