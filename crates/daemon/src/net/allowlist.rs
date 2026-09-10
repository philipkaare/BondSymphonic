//! Which hosts a sandboxed workspace is allowed to reach.
//!
//! A pattern is either an exact host (`api.anthropic.com`) or a suffix wildcard
//! (`*.npmjs.org`, which covers `registry.npmjs.org` and `a.b.npmjs.org` but
//! neither `npmjs.org` itself nor `evilnpmjs.org`). Ports are unrestricted: the
//! proxy decides *where* a connection may go, not on which port, because a
//! registry that moves to 8443 is still the same trust decision.
//!
//! Everything here is pure and synchronous, so the proxy can consult it on the
//! connection path without locking or awaiting, and so the rules can be tested
//! without a sandbox.

use crate::runs::config::RepoConfig;

/// The hosts every workspace may reach without any configuration: the Anthropic
/// API the agent talks to, and the package registries a repo has to fetch from
/// to build at all.
///
/// Kept in the daemon spec's order (§7.1) so the two lists can be diffed by
/// eye. `crates.io`, `static.crates.io` and `index.crates.io` are listed
/// separately rather than folded into `*.crates.io`, because the wildcard would
/// also open every future subdomain of a registry we need three names from.
pub const DEFAULT_ALLOW: [&str; 12] = [
    "api.anthropic.com",
    "*.anthropic.com",
    "registry.npmjs.org",
    "*.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
];

/// One allowlist entry, already lowercased and validated.
///
/// The inner string keeps the `*.` prefix of a wildcard rather than splitting
/// into a flag and a suffix, so [`Allowlist::to_strings`] can hand the exact
/// text back to the IDE and the registry without reconstructing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPattern(String);

impl HostPattern {
    /// Parses one entry, lowercasing it.
    ///
    /// Rejects anything that is not a bare host or a `*.`-prefixed suffix: an
    /// empty or blank entry, a lone `*` (which would allow everything), a
    /// wildcard anywhere but the front (`foo.*`, and `*bar.com` — the latter
    /// would otherwise let `evilnpmjs.org` through as `*npmjs.org`), and
    /// anything carrying a scheme, path or port (`/`, `:`), which never appears
    /// in the host being matched and so would silently match nothing.
    pub fn parse(s: &str) -> Result<HostPattern, String> {
        let lower = s.to_ascii_lowercase();
        if lower.is_empty() {
            return Err("host pattern is empty".into());
        }
        if lower.chars().any(char::is_whitespace) {
            return Err(format!("host pattern {s:?} contains whitespace"));
        }
        if lower.contains('/') || lower.contains(':') {
            return Err(format!(
                "host pattern {s:?} must be a bare host, without a scheme, path or port"
            ));
        }
        // A wildcard is only ever the whole first label. What follows it is
        // matched literally, so it has to be a plain host in its own right.
        let literal = lower.strip_prefix("*.").unwrap_or(&lower);
        if literal.is_empty() {
            return Err(format!("host pattern {s:?} has nothing after the wildcard"));
        }
        if literal.contains('*') {
            return Err(format!(
                "host pattern {s:?} may only wildcard a whole leading label, as *.example.com"
            ));
        }
        Ok(HostPattern(lower))
    }

    /// Whether `host` is covered by this pattern. Case-insensitive; a wildcard
    /// needs at least one label in front of its suffix, so `*.npmjs.org` covers
    /// `registry.npmjs.org` but not `npmjs.org`.
    pub fn matches(&self, host: &str) -> bool {
        let host = normalize_host(host);
        match self.0.strip_prefix("*.") {
            // The remaining head must be a non-empty label sequence ending in
            // the separating dot: that is what keeps `evilnpmjs.org` out, where
            // the suffix matches but the character in front of it is not a dot.
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
            None => host == self.0,
        }
    }

    /// The pattern as written in `bondsymphonic.toml` and stored in the
    /// workspace registry.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A host as it is compared: lowercased, with the trailing dot of a fully
/// qualified name dropped, so `github.com.` and `github.com` are one host.
///
/// Ports are deliberately *not* stripped here. Splitting `host:port` is the
/// proxy's job, where an IPv6 literal (`[::1]:8080`) still has to be told from
/// a bare address; a pattern carrying a colon is rejected at parse time instead.
fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    h.strip_suffix('.').unwrap_or(&h).to_string()
}

/// The set of patterns one workspace may reach, in the order they were
/// configured.
///
/// Matching walks the list. At a dozen or so entries that beats any index, and
/// it keeps the order the user wrote, so a denial can quote the list back.
#[derive(Clone, Debug, Default)]
pub struct Allowlist {
    patterns: Vec<HostPattern>,
}

impl Allowlist {
    /// Builds the list, dropping entries that are not valid patterns.
    ///
    /// A typo in `bondsymphonic.toml` must not take the whole allowlist with
    /// it: rejecting the list would strand the workspace with no network, and
    /// failing open would hand it every host. Each bad entry is warned about
    /// and skipped, so the rest of the list still applies.
    pub fn from_strings(items: &[String]) -> Allowlist {
        let mut patterns = Vec::with_capacity(items.len());
        for item in items {
            match HostPattern::parse(item) {
                Ok(p) => patterns.push(p),
                Err(e) => tracing::warn!(entry = %item, error = %e, "ignoring allowlist entry"),
            }
        }
        Allowlist { patterns }
    }

    /// Whether any pattern covers `host`.
    pub fn allows(&self, host: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(host))
    }

    /// The patterns as text, in order, for the registry and the IDE.
    pub fn to_strings(&self) -> Vec<String> {
        self.patterns.iter().map(|p| p.0.clone()).collect()
    }

    /// How many patterns the list holds.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether the list is empty, in which case nothing is reachable.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// The allowlist a new workspace starts with: the defaults, then whatever the
/// repo's `bondsymphonic.toml` `[network] allow` adds.
///
/// The repo *extends* the defaults rather than replacing them, so a project
/// that needs its own artifact host does not have to re-list npm and PyPI, and
/// cannot accidentally cut its agent off from `api.anthropic.com`. Entries are
/// canonicalised through [`HostPattern`] and de-duplicated keeping the first
/// occurrence's position, so a repo that repeats `github.com` neither doubles
/// it nor moves it down the list. Runtime changes go through
/// `workspace.set_allowlist`, which replaces the effective list outright.
pub fn effective(repo: Option<&RepoConfig>) -> Vec<String> {
    let extra = repo.map(|r| r.network.allow.as_slice()).unwrap_or(&[]);
    let mut out: Vec<String> = Vec::with_capacity(DEFAULT_ALLOW.len() + extra.len());
    let configured = Allowlist::from_strings(extra).to_strings();
    for item in DEFAULT_ALLOW
        .iter()
        .map(|s| s.to_string())
        .chain(configured)
    {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_and_wildcard_patterns_match_case_insensitively() {
        let p = HostPattern::parse("Api.Anthropic.com").unwrap();
        assert!(p.matches("api.anthropic.com"));
        assert!(!p.matches("anthropic.com"));
        let w = HostPattern::parse("*.npmjs.org").unwrap();
        assert!(w.matches("registry.npmjs.org"));
        assert!(w.matches("a.b.npmjs.org"));
        assert!(!w.matches("npmjs.org"));
        assert!(!w.matches("evilnpmjs.org"));
    }
    #[test]
    fn invalid_patterns_are_rejected() {
        for bad in ["", " ", "*", "foo.*", "a/b", "host:80", "*.", "*bar.com"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?}");
        }
    }
    #[test]
    fn default_list_allows_the_registries_and_nothing_else() {
        let a = Allowlist::from_strings(&DEFAULT_ALLOW.map(String::from));
        for ok in [
            "api.anthropic.com",
            "registry.npmjs.org",
            "index.crates.io",
            "raw.githubusercontent.com",
            "github.com",
        ] {
            assert!(a.allows(ok), "{ok}");
        }
        for no in ["example.com", "evil-github.com", "githubusercontent.com"] {
            assert!(!a.allows(no), "{no}");
        }
    }
    #[test]
    fn effective_list_extends_defaults_without_duplicates() {
        let cfg = crate::runs::config::RepoConfig {
            network: crate::runs::config::NetworkSection {
                allow: vec!["*.mycompany.com".into(), "github.com".into()],
            },
            ..Default::default()
        };
        let v = effective(Some(&cfg));
        assert_eq!(v.len(), DEFAULT_ALLOW.len() + 1);
        assert_eq!(v.last().unwrap(), "*.mycompany.com");
        assert_eq!(effective(None).len(), DEFAULT_ALLOW.len());
    }

    /// The list is what the sandbox is held to, so the ways a host can be
    /// dressed up to look like an allowed one all have to fail closed.
    #[test]
    fn near_misses_and_odd_spellings_are_handled() {
        let a = Allowlist::from_strings(&DEFAULT_ALLOW.map(String::from));
        // Case and the trailing dot of a fully qualified name are the same host.
        assert!(a.allows("REGISTRY.NPMJS.ORG"));
        assert!(a.allows("github.com."));
        // A suffix that is not a label boundary, and a host that merely
        // contains an allowed one, are both denied.
        assert!(!a.allows("notgithub.com"));
        assert!(!a.allows("github.com.evil.example"));
        // An entry the parser refuses is dropped rather than taking the list
        // down with it, and the surviving entries still match.
        let mixed = Allowlist::from_strings(&["*".to_string(), "ok.example".to_string()]);
        assert_eq!(mixed.to_strings(), vec!["ok.example".to_string()]);
        assert!(mixed.allows("ok.example"));
        assert!(!mixed.allows("anything.else"));
        // An empty list reaches nothing at all.
        assert!(Allowlist::default().is_empty());
        assert!(!Allowlist::default().allows("github.com"));
    }
}
