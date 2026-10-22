//! The MCP servers settings, independent of how they are drawn: which
//! `mcp.list` reply is the one to show, how the server editor's write-only
//! secret fields become [`McpSecretWrite`]s, and which OAuth sign-in a browser
//! callback belongs to.
//!
//! The daemon owns the registry. Everything here is the IDE's view of it and
//! the checks worth making before a request goes out, so a mistyped ID is said
//! beside the field rather than coming back as `invalid_params`.

use bondsymphonic_proto::{
    McpListResult, McpSecretValue, McpSecretWrite, McpServerConfig, McpTransportConfig,
    McpUpsertParams, McpWorkspaceOverride, SecretRef,
};
use serde::Deserialize;

/// The server ID the daemon keeps for its own git tools.
pub const RESERVED_SERVER_ID: &str = "bondsymphonic";

/// The latest `mcp.list` answer, and enough bookkeeping to tell a stale one.
///
/// `mcp.changed` events may arrive duplicated or out of order, and every one
/// re-lists, so several requests can be in flight and their replies can come
/// back in any order. A reply is shown when its registry revision is newer
/// than the one on screen, or when it is the same revision and was asked for
/// later -- a status change (a login finishing, a test failing) leaves the
/// revision where it was, and the later request saw the later statuses.
#[derive(Debug, Default)]
pub struct McpView {
    next_ticket: u64,
    /// The ticket of the reply on screen, list or error.
    shown_ticket: u64,
    revision: Option<u64>,
    listing: Option<McpListResult>,
    /// Why the registry could not be listed: a degraded daemon registry
    /// answers with an error naming `registry.json`, which the page shows in
    /// place of the list.
    error: Option<String>,
}

impl McpView {
    /// A ticket for one `mcp.list` request, handed back with its reply.
    pub fn begin_list(&mut self) -> u64 {
        self.next_ticket += 1;
        self.next_ticket
    }

    /// Shows `result` unless a newer one is already shown. True when it was.
    pub fn apply_list(&mut self, ticket: u64, result: McpListResult) -> bool {
        let revision = result.registry.revision;
        let newer = match self.revision {
            None => true,
            Some(shown) => revision > shown || (revision == shown && ticket > self.shown_ticket),
        };
        if !newer {
            return false;
        }
        self.revision = Some(revision);
        self.shown_ticket = self.shown_ticket.max(ticket);
        self.listing = Some(result);
        self.error = None;
        true
    }

    /// Shows a listing failure, unless a later request has already answered.
    pub fn apply_error(&mut self, ticket: u64, message: String) -> bool {
        if ticket <= self.shown_ticket {
            return false;
        }
        self.shown_ticket = ticket;
        self.error = Some(message);
        true
    }

    /// Forgets everything: a new connection may be to another daemon, whose
    /// revisions have nothing to do with the last one's.
    pub fn reset(&mut self) {
        *self = Self {
            next_ticket: self.next_ticket,
            ..Self::default()
        };
    }

    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    pub fn listing(&self) -> Option<&McpListResult> {
        self.listing.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// What the page is drawn from: the `McpListResult` as JSON, `{"error":
    /// ...}` when the registry could not be listed, or `{}` before any answer.
    pub fn to_json(&self) -> String {
        if let Some(error) = &self.error {
            return serde_json::json!({ "error": error }).to_string();
        }
        match &self.listing {
            Some(listing) => serde_json::to_string(listing).unwrap_or_else(|_| "{}".into()),
            None => "{}".into(),
        }
    }
}

/// One secret field of the server editor, as the dialog hands it over.
///
/// The editor never has the stored value: it is not in any listing. So an
/// untouched field is blank, and blank means keep what is stored.
#[derive(Clone, Deserialize)]
pub struct SecretField {
    pub reference: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub remove: bool,
}

/// What the server editor saves: the configuration, its secret fields and the
/// registry revision it was opened at, so an edit made on a stale view is
/// refused rather than overwriting a newer one.
#[derive(Deserialize)]
pub struct ServerForm {
    pub server: McpServerConfig,
    #[serde(default)]
    pub secrets: Vec<SecretField>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}

/// Remove when asked, Set when a value was typed, and Keep for a blank field.
/// Whitespace counts as blank, as the daemon reads it.
pub fn secret_write(field: &SecretField) -> McpSecretWrite {
    let reference = SecretRef(field.reference.clone());
    if field.remove {
        McpSecretWrite::Remove { reference }
    } else if field.value.trim().is_empty() {
        McpSecretWrite::Keep { reference }
    } else {
        McpSecretWrite::Set {
            reference,
            value: McpSecretValue(field.value.clone()),
        }
    }
}

/// `^[a-z0-9][a-z0-9_-]{0,31}$`, the daemon's rule for a server ID.
pub fn valid_server_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`, a name the daemon will put in an env file.
pub fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// The `mcp.upsert` the editor's form asks for, or the sentence saying what
/// to fix. The daemon checks all of this again; these are the mistakes worth
/// catching beside the field.
pub fn upsert_params(form: ServerForm) -> Result<McpUpsertParams, String> {
    let id = form.server.id.as_str();
    if id == RESERVED_SERVER_ID {
        return Err(format!(
            "The ID \"{RESERVED_SERVER_ID}\" is reserved for the built-in git tools."
        ));
    }
    if !valid_server_id(id) {
        return Err("The ID must be 1 to 32 lowercase letters, digits, - or _, \
                    starting with a letter or digit."
            .into());
    }
    if form.server.name.trim().is_empty() {
        return Err("Give the server a name.".into());
    }
    match &form.server.transport {
        McpTransportConfig::Stdio {
            command,
            env,
            secret_env,
            ..
        } => {
            if command.trim().is_empty() {
                return Err("Enter the command to run in the sandbox.".into());
            }
            if let Some(bad) = env
                .keys()
                .chain(secret_env.iter())
                .find(|name| !valid_env_name(name))
            {
                return Err(format!(
                    "\"{bad}\" is not a valid environment variable name: use letters, \
                     digits and _, not starting with a digit."
                ));
            }
            if let Some(both) = secret_env.iter().find(|name| env.contains_key(*name)) {
                return Err(format!(
                    "{both} is both a plain and a secret variable; keep one."
                ));
            }
        }
        McpTransportConfig::Http { url, .. } => {
            if !url.starts_with("https://") {
                return Err("Remote servers need an https:// URL.".into());
            }
        }
    }
    Ok(McpUpsertParams {
        secrets: form.secrets.iter().map(secret_write).collect(),
        server: form.server,
        expected_revision: form.expected_revision,
    })
}

/// Whether an agent started in a workspace with `value` gets `server`: the
/// rule the daemon resolves at agent start, for the workspace dialog to say
/// what "Inherit" currently means.
pub fn effective_in_workspace(server: &McpServerConfig, value: McpWorkspaceOverride) -> bool {
    server.enabled
        && match value {
            McpWorkspaceOverride::Inherit => server.default_workspace_enabled,
            McpWorkspaceOverride::Enabled => true,
            McpWorkspaceOverride::Disabled => false,
        }
}

/// The browser sign-ins in flight: at most one, because the newest Connect
/// replaces any earlier one, and the daemon itself keeps one pending login
/// per server.
///
/// An attempt learns its `state` from the authorization URL the daemon
/// returns, and the loopback listener hands the first callback carrying that
/// state over exactly once. A second callback, or one with any other state,
/// belongs to no attempt and is ignored.
#[derive(Debug, Default)]
pub struct OAuthAttempts {
    next: u64,
    active: Option<Attempt>,
}

#[derive(Debug)]
struct Attempt {
    id: u64,
    server_id: String,
    state: Option<String>,
}

impl OAuthAttempts {
    /// Starts an attempt for `server_id`, ending any other.
    pub fn begin(&mut self, server_id: &str) -> u64 {
        self.next += 1;
        self.active = Some(Attempt {
            id: self.next,
            server_id: server_id.to_owned(),
            state: None,
        });
        self.next
    }

    /// Records the `state` the daemon put in `attempt`'s authorization URL.
    /// False when the attempt has since ended.
    pub fn set_state(&mut self, attempt: u64, state: String) -> bool {
        match &mut self.active {
            Some(active) if active.id == attempt => {
                active.state = Some(state);
                true
            }
            _ => false,
        }
    }

    pub fn is_active(&self, attempt: u64) -> bool {
        self.active.as_ref().is_some_and(|a| a.id == attempt)
    }

    /// The server a callback with `state` signs in to, ending the attempt;
    /// `None` when the attempt is over or the state is not its own.
    pub fn take(&mut self, attempt: u64, state: &str) -> Option<String> {
        let matches = self
            .active
            .as_ref()
            .is_some_and(|a| a.id == attempt && a.state.as_deref() == Some(state));
        if !matches {
            return None;
        }
        self.active.take().map(|a| a.server_id)
    }

    /// Ends `attempt` if it is still the active one.
    pub fn cancel(&mut self, attempt: u64) {
        if self.is_active(attempt) {
            self.active = None;
        }
    }

    /// Ends whatever attempt there is: the connection it was made on is gone.
    pub fn cancel_all(&mut self) {
        self.active = None;
    }

    /// The server the active attempt is for.
    pub fn server(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.server_id.as_str())
    }
}
