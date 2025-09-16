use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct EtherscanAbiResponse {
    pub status: String,
    pub message: String,
    pub result: String,
}