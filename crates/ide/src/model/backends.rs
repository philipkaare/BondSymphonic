//! Backend defaults and reply identity, independent of Qt widgets.
use crate::qobjects::settings::Settings;
use bondsymphonic_proto::{
    AgentAdapterKind, AgentStartOptions, BackendChoice, BackendDescriptor, SetupAction,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BackendSettings {
    pub enabled: bool,
    pub default_model: String,
    pub default_permission_mode: String,
}

pub fn default_settings() -> BTreeMap<String, BackendSettings> {
    [
        ("claude", true, "bypassPermissions"),
        ("codex", false, "never"),
    ]
    .into_iter()
    .map(|(id, enabled, mode)| {
        (
            id.into(),
            BackendSettings {
                enabled,
                default_model: String::new(),
                default_permission_mode: mode.into(),
            },
        )
    })
    .collect()
}
pub fn word(kind: AgentAdapterKind) -> &'static str {
    match kind {
        AgentAdapterKind::Claude => "claude",
        AgentAdapterKind::Codex => "codex",
        AgentAdapterKind::Terminal => "terminal",
    }
}
pub fn parse(word: &str) -> Option<AgentAdapterKind> {
    match word {
        "claude" => Some(AgentAdapterKind::Claude),
        "codex" => Some(AgentAdapterKind::Codex),
        "terminal" => Some(AgentAdapterKind::Terminal),
        _ => None,
    }
}
pub fn credential_name(kind: AgentAdapterKind) -> Option<&'static str> {
    match kind {
        AgentAdapterKind::Claude => Some("anthropic_api_key"),
        AgentAdapterKind::Codex => Some("openai_api_key"),
        AgentAdapterKind::Terminal => None,
    }
}

pub fn permission_reply(
    kind: AgentAdapterKind,
    allow: bool,
    session: bool,
) -> (bondsymphonic_proto::PermissionDecision, bool) {
    use bondsymphonic_proto::PermissionDecision;
    if !allow {
        return (PermissionDecision::Deny, false);
    }
    if kind == AgentAdapterKind::Codex && session {
        return (PermissionDecision::AllowForSession, false);
    }
    (
        PermissionDecision::Allow,
        session && kind == AgentAdapterKind::Claude,
    )
}

pub fn select_default_backend(
    settings: &Settings,
    descriptors: &[BackendDescriptor],
) -> Option<AgentAdapterKind> {
    let enabled =
        |d: &&BackendDescriptor| settings.backends.get(word(d.id)).is_some_and(|s| s.enabled);
    descriptors
        .iter()
        .filter(enabled)
        .find(|d| word(d.id) == settings.default_backend)
        .or_else(|| descriptors.iter().find(enabled))
        .map(|d| d.id)
}

pub fn start_options_with_defaults(
    raw: &str,
    defaults: &BackendSettings,
) -> Result<AgentStartOptions, String> {
    let mut options: AgentStartOptions =
        serde_json::from_str(if raw.trim().is_empty() { "{}" } else { raw })
            .map_err(|e| e.to_string())?;
    if options.model.is_none() && !defaults.default_model.is_empty() {
        options.model = Some(defaults.default_model.clone());
    }
    if options.permission_mode.is_none() && !defaults.default_permission_mode.is_empty() {
        options.permission_mode = Some(defaults.default_permission_mode.clone());
    }
    // Only the controller may add a credential after resolving the backend.
    options.api_key = None;
    Ok(options)
}

/// Missing identity belongs to a legacy Claude tab, never the current default.
pub fn resolve_start(
    raw: &str,
    settings: &Settings,
    descriptors: &[BackendDescriptor],
) -> Result<(AgentAdapterKind, AgentStartOptions), String> {
    let value: serde_json::Value =
        serde_json::from_str(if raw.trim().is_empty() { "{}" } else { raw })
            .map_err(|e| e.to_string())?;
    let id = value
        .get("adapter")
        .and_then(|v| v.as_str())
        .unwrap_or("claude");
    let kind = parse(id)
        .filter(|k| *k != AgentAdapterKind::Terminal)
        .ok_or_else(|| format!("Unknown agent backend: {id}"))?;
    if !descriptors.iter().any(|d| d.id == kind) {
        return Err(format!("The connected daemon does not support {id}"));
    }
    let defaults = settings
        .backends
        .get(id)
        .filter(|s| s.enabled)
        .ok_or_else(|| format!("Enable {id} in Settings before starting an agent"))?;
    Ok((kind, start_options_with_defaults(raw, defaults)?))
}

pub fn legacy_descriptors() -> Vec<BackendDescriptor> {
    vec![BackendDescriptor{id:AgentAdapterKind::Claude,label:"Claude".into(),permission_modes:[("bypassPermissions","YOLO (sandboxed)"),("acceptEdits","Accept edits (other tools blocked)"),("plan","Plan only"),("manual","Ask every time (blocks instead)")].into_iter().map(|(id,label)|BackendChoice{id:id.into(),label:label.into()}).collect(),default_permission_mode:"bypassPermissions".into(),permission_note:"Claude Code cannot reach this window to ask, so a tool that needs approval is refused rather than queued. YOLO runs everything — the sandbox, the worktree and the network proxy are what make that reasonable.".into(),credential_label:"Anthropic API key".into(),prerequisite_names:vec!["claude".into(),"claude_auth".into()],setup_actions:vec![SetupAction::InstallClaude,SetupAction::ClaudeLogin,SetupAction::ClaudeLogout,SetupAction::ClaudeSetupToken]}]
}

#[derive(Default)]
pub struct ReplyGenerations {
    next: u64,
    current: BTreeMap<String, u64>,
}
impl ReplyGenerations {
    pub fn begin(&mut self, backend: &str) -> u64 {
        self.next += 1;
        self.current.insert(backend.into(), self.next);
        self.next
    }
    pub fn accepts(&self, backend: &str, generation: u64) -> bool {
        self.current.get(backend) == Some(&generation)
    }
    pub fn disconnect(&mut self) {
        self.current.clear();
    }
}
