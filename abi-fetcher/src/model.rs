use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ExplorerAbiResponse {
    pub status: String,
    pub message: String,
    pub result: String,
}
