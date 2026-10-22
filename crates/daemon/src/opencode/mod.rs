//! Provider configuration contains no credentials. Keys stay in the IDE credential store.
use bondsymphonic_proto::opencode::*;
use bondsymphonic_proto::{ErrorCode, RpcError};
use parking_lot::RwLock;
use std::net::IpAddr;

#[derive(Default)]
pub struct ProviderStore {
    launch_edit: tokio::sync::Mutex<()>,
    state: RwLock<ProviderState>,
    path: Option<std::path::PathBuf>,
}
/// Public revision remains global; discovery invalidation is local to each provider.
#[derive(Default, Clone)]
struct ProviderState {
    snapshot: ProviderSnapshot,
    generations: std::collections::HashMap<String, u64>,
}
#[derive(Debug, Clone)]
pub struct DiscoveryTicket {
    provider_id: String,
    provider_generation: u64,
    credential_generation: u64,
    connection_generation: u64,
}
impl ProviderStore {
    /// Hold across preparation/grant publication or configuration edit/revocation.
    pub async fn lock_launch_edit(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.launch_edit.lock().await
    }
    pub fn load(root: std::path::PathBuf) -> Result<Self, RpcError> {
        let path = root.join("opencode-providers.json");
        let snapshot = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| RpcError::invalid_params("invalid provider configuration file"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProviderSnapshot::default(),
            Err(e) => return Err(RpcError::io(&e)),
        };
        let mut store = Self::from_snapshot(snapshot)?;
        store.path = Some(path);
        Ok(store)
    }
    fn persist(&self, snapshot: &ProviderSnapshot) -> Result<(), RpcError> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
            }
            let bytes = serde_json::to_vec_pretty(snapshot)
                .map_err(|_| RpcError::internal("provider serialization failed"))?;
            crate::util::atomic::write_atomic(path, &bytes).map_err(|e| RpcError::io(&e))?;
        }
        Ok(())
    }
    /// Integration must revoke companion grants after this returns successfully.
    pub fn remove(&self, expected_revision: u64, id: &str) -> Result<ProviderSnapshot, RpcError> {
        let mut state = self.state.write();
        if state.snapshot.revision != expected_revision {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                "provider configuration changed; reload before saving",
            ));
        }
        if !state.snapshot.providers.iter().any(|p| p.id == id) {
            return Err(RpcError::not_found("provider not configured"));
        }
        let mut candidate = state.clone();
        candidate.snapshot.providers.retain(|p| p.id != id);
        candidate.snapshot.revision = candidate
            .snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| RpcError::internal("provider revision exhausted"))?;
        candidate.generations.remove(id);
        self.persist(&candidate.snapshot)?;
        *state = candidate;
        Ok(state.snapshot.clone())
    }
    pub fn snapshot(&self) -> ProviderSnapshot {
        self.state.read().snapshot.clone()
    }
    pub fn from_snapshot(snapshot: ProviderSnapshot) -> Result<Self, RpcError> {
        let mut ids = std::collections::HashSet::new();
        for p in &snapshot.providers {
            validate(p)?;
            if !ids.insert(&p.id) {
                return Err(RpcError::invalid_params("duplicate provider ID"));
            }
        }
        Ok(Self {
            launch_edit: tokio::sync::Mutex::new(()),
            state: RwLock::new(ProviderState {
                generations: snapshot
                    .providers
                    .iter()
                    .map(|p| (p.id.clone(), snapshot.revision))
                    .collect(),
                snapshot,
            }),
            path: None,
        })
    }
    pub fn upsert(
        &self,
        expected_revision: u64,
        provider: OpenCodeProvider,
    ) -> Result<ProviderSnapshot, RpcError> {
        validate(&provider)?;
        let mut s = self.state.write();
        if s.snapshot.revision != expected_revision {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                "provider configuration changed; reload before saving",
            ));
        }
        let provider_id = provider.id.clone();
        let mut candidate = s.clone();
        let next = s
            .snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| RpcError::internal("provider revision exhausted"))?;
        if let Some(p) = candidate
            .snapshot
            .providers
            .iter_mut()
            .find(|p| p.id == provider.id)
        {
            *p = provider;
        } else {
            candidate.snapshot.providers.push(provider);
        }
        candidate.snapshot.revision = next;
        candidate.generations.insert(provider_id, next);
        self.persist(&candidate.snapshot)?;
        *s = candidate;
        Ok(s.snapshot.clone())
    }
    pub fn resolve(&self, id: &str) -> Result<OpenCodeProvider, RpcError> {
        self.state
            .read()
            .snapshot
            .providers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| {
                RpcError::not_found("provider not configured; add an OpenCode provider in Settings")
            })
    }
    pub fn discovery_ticket(
        &self,
        id: &str,
        credential_generation: u64,
        connection_generation: u64,
    ) -> Result<DiscoveryTicket, RpcError> {
        self.discovery_context(id, credential_generation, connection_generation)
            .map(|(_, ticket)| ticket)
    }
    /// The endpoint and invalidation ticket must describe the same snapshot.
    pub fn discovery_context(
        &self,
        id: &str,
        credential_generation: u64,
        connection_generation: u64,
    ) -> Result<(OpenCodeProvider, DiscoveryTicket), RpcError> {
        let state = self.state.read();
        let provider = state
            .snapshot
            .providers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| RpcError::not_found("provider not configured"))?;
        let ticket = DiscoveryTicket {
            provider_id: id.into(),
            provider_generation: *state
                .generations
                .get(id)
                .expect("configured provider has a generation"),
            credential_generation,
            connection_generation,
        };
        Ok((provider, ticket))
    }
    /// Caller supplies the current credential and connection generations, never values from the reply.
    pub fn apply_discovery(
        &self,
        t: &DiscoveryTicket,
        credential_generation: u64,
        connection_generation: u64,
        models: Vec<OpenCodeModel>,
    ) -> Result<bool, RpcError> {
        for m in &models {
            validate_model(m)?;
        }
        let mut s = self.state.write();
        if s.generations.get(&t.provider_id) != Some(&t.provider_generation)
            || t.credential_generation != credential_generation
            || t.connection_generation != connection_generation
        {
            return Ok(false);
        }
        let mut candidate = s.clone();
        let next = s
            .snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| RpcError::internal("provider revision exhausted"))?;
        let Some(p) = candidate
            .snapshot
            .providers
            .iter_mut()
            .find(|p| p.id == t.provider_id)
        else {
            return Ok(false);
        };
        // Discovery supplements explicit models; missing metadata cannot erase manual limits.
        for m in models {
            validate_model(&m)?;
            if !p.models.iter().any(|old| old.id == m.id) {
                p.models.push(m);
            }
        }
        candidate.snapshot.revision = next;
        candidate.generations.insert(t.provider_id.clone(), next);
        self.persist(&candidate.snapshot)?;
        *s = candidate;
        Ok(true)
    }
}
/// Match the proxy's IPv4-mapped IPv6 classification before namespace checks.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}

fn validate_model(m: &OpenCodeModel) -> Result<(), RpcError> {
    if m.id.trim().is_empty()
        || m.id.chars().any(char::is_control)
        || m.context_limit == Some(0)
        || m.output_limit == Some(0)
    {
        return Err(RpcError::invalid_params(
            "model ID and optional limits must be nonempty and positive",
        ));
    }
    Ok(())
}
fn validate(p: &OpenCodeProvider) -> Result<(), RpcError> {
    if p.id.is_empty()
        || p.id.len() > 128
        || !p
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || p.name.trim().is_empty()
    {
        return Err(RpcError::invalid_params(
            "provider requires a stable ID and display name",
        ));
    }
    ProviderEndpoint::parse(p)?;
    let mut ids = std::collections::HashSet::new();
    for m in &p.models {
        validate_model(m)?;
        if !ids.insert(&m.id) {
            return Err(RpcError::invalid_params("duplicate model ID"));
        }
    }
    Ok(())
}
#[derive(Debug, Clone)]
pub struct ProviderEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub target: EndpointTarget,
}
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    /// Verified Windows host address from the daemon's actual WSL routing configuration.
    pub windows_host: Option<IpAddr>,
}
impl ProviderEndpoint {
    pub fn parse(p: &OpenCodeProvider) -> Result<Self, RpcError> {
        let invalid = || {
            RpcError::invalid_params("provider URL must be HTTP(S) with a valid host and port, without credentials, query, fragment or whitespace")
        };
        let url = &p.base_url;
        if url.chars().any(|c| c.is_whitespace() || c.is_control())
            || url.contains(['@', '?', '#', '\\'])
        {
            return Err(invalid());
        }
        let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
        if !matches!(scheme, "http" | "https") {
            return Err(invalid());
        }
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, String::new()), |(a, b)| (a, format!("/{b}")));
        let (host, port_text) = if let Some(a) = authority.strip_prefix('[') {
            let (h, tail) = a.split_once(']').ok_or_else(invalid)?;
            if h.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(invalid());
            }
            (
                h,
                if tail.is_empty() {
                    None
                } else {
                    Some(tail.strip_prefix(':').ok_or_else(invalid)?)
                },
            )
        } else {
            if authority.matches(':').count() > 1 {
                return Err(invalid());
            }
            authority
                .split_once(':')
                .map_or((authority, None), |(h, p)| (h, Some(p)))
        };
        if !crate::net::proxy::is_valid_host(host) {
            return Err(invalid());
        }
        let port = match port_text {
            Some(p) => {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                p.parse::<u16>().map_err(|_| invalid())?
            }
            None => {
                if scheme == "https" {
                    443
                } else {
                    80
                }
            }
        };
        if port == 0 {
            return Err(invalid());
        }
        Ok(Self {
            scheme: scheme.into(),
            host: crate::net::allowlist::normalize_host(host),
            port,
            path,
            target: p.endpoint_target,
        })
    }
    fn url_with_host(&self, host: &str) -> String {
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.into()
        };
        format!("{}://{}:{}{}", self.scheme, host, self.port, self.path)
    }
    pub fn models_url(&self) -> String {
        format!(
            "{}/models",
            self.url_with_host(&self.host).trim_end_matches('/')
        )
    }
    pub fn routed_url(&self, routes: &RoutingContext) -> Result<String, RpcError> {
        let loopback = self.host == "localhost"
            || self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|a| canonical_ip(a).is_loopback());
        match self.target {
            EndpointTarget::Windows => {
                let addr=routes.windows_host.map(canonical_ip).ok_or_else(||RpcError::new(ErrorCode::PrereqMissing,"Windows model routing is unavailable; configure and verify the Windows host address reachable from WSL"))?;
                if addr.is_unspecified() || addr.is_multicast() {
                    return Err(RpcError::invalid_params(
                        "Windows route must name a reachable unicast address",
                    ));
                }
                if !loopback {
                    return Err(RpcError::invalid_params("Windows target requires a loopback provider URL; use Remote for an explicit network address"));
                }
                Ok(self.url_with_host(&addr.to_string()))
            }
            EndpointTarget::Wsl => Ok(self.url_with_host(if self.host == "localhost" {
                "127.0.0.1"
            } else {
                &self.host
            })),
            EndpointTarget::Remote if loopback => Err(RpcError::invalid_params(
                "Remote target cannot use localhost; choose WSL host or Windows host",
            )),
            EndpointTarget::Remote => Ok(self.url_with_host(&self.host)),
        }
    }
}
