use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointTarget {
    Wsl,
    Windows,
    Remote,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCodeModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_limit: Option<u64>,
    #[serde(default)]
    pub output_limit: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCodeProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub endpoint_target: EndpointTarget,
    #[serde(default)]
    pub models: Vec<OpenCodeModel>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub revision: u64,
    pub providers: Vec<OpenCodeProvider>,
}
