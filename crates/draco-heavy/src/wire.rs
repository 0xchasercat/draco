use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::RenderMode;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintRequest {
    pub url: String,
    pub proxy: String,
    #[serde(default)]
    pub render_opts: Option<Map<String, Value>>,
    #[serde(default)]
    pub wait_strategy: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintSuccess {
    pub success: bool,
    pub final_url: String,
    pub cookies: HashMap<String, String>,
    pub html: String,
    pub markdown: String,
    pub render_mode: RenderMode,
    pub ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub success: bool,
    pub error: String,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: error.into(),
        }
    }
}
