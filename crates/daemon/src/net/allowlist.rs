//! Which hosts a sandboxed workspace is allowed to reach.
//!
//! A pattern is either an exact host (`api.anthropic.com`, or an address such
//! as `127.0.0.1` or `2606:4700::1111`) or a suffix wildcard (`*.npmjs.org`,
//! which covers `registry.npmjs.org` and `a.b.npmjs.org` but neither
//! `npmjs.org` itself nor `evilnpmjs.org`). Ports are unrestricted: the proxy
//! decides *where* a connection may go, not on which port, because a registry
//! that moves to 8443 is still the same trust decision.
//!
//! Every entry and every host is compared in one normalised form
//! ([`normalize_host`]): lowercased, without the root dot of a fully qualified
//! name, and with an IPv6 literal in its canonical spelling and without the
//! brackets a URI wraps it in. A denial the proxy publishes carries that same
//! form, so the entry a one-click "Allow host" writes back is the one the next
//! request matches.
//!
//! Everything here is pure and synchronous, so the proxy can consult it on the
//! connection path without locking or awaiting, and so the rules can be tested
//! without a sandbox.

use crate::runs::config::RepoConfig;
use std::net::Ipv6Addr;

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
    /// Parses one entry into its normalised form: lowercased, without the root
    /// dot of a fully qualified name, an IPv6 literal in its canonical spelling
    /// and without brackets.
    ///
    /// Rejects anything that is not a bare host or a `*.`-prefixed suffix: an
    /// empty or blank entry, a lone `*` (which would allow everything), a
    /// wildcard anywhere but the front (`foo.*`, and `*bar.com` — the latter
    /// would otherwise let `evilnpmjs.org` through as `*npmjs.org`), an empty
    /// label (`a..b`, a leading dot, two root dots), and anything carrying a
    /// scheme, path or port (`/`, or a `:` that is not part of an IPv6
    /// address), which never appears in the host being matched and so would
    /// silently match nothing.
    pub fn parse(s: &str) -> Result<HostPattern, String> {
        let lower = s.to_ascii_lowercase();
        if lower.is_empty() {
            return Err("host pattern is empty".into());
        }
        if lower.chars().any(char::is_whitespace) {
            return Err(format!("host pattern {s:?} contains whitespace"));
        }
        // An IPv6 address is the one host with colons in it, and in a URI it
        // wears brackets that are not part of the address. Either spelling is
        // the address, stored canonically so `2606:4700:0:0::1111` and
        // `2606:4700::1111` are one entry.
        if let Some(v6) = ipv6_literal(&lower) {
            return Ok(HostPattern(v6.to_string()));
        }
        if lower.contains('[') || lower.contains(']') {
            return Err(format!(
                "host pattern {s:?} is bracketed but is not an IPv6 address"
            ));
        }
        if lower.contains('/') || lower.contains(':') {
            return Err(format!(
                "host pattern {s:?} must be a bare host, without a scheme, path or port"
            ));
        }
        // One trailing dot is the root label of a fully qualified name, and
        // the same host without it. What remains must have no empty label: a
        // second dot, a leading dot, or `a..b` name nothing.
        let lower = lower.strip_suffix('.').unwrap_or(&lower).to_string();
        if lower.is_empty()
            || lower.starts_with('.')
            || lower.ends_with('.')
            || lower.contains("..")
        {
            return Err(format!("host pattern {s:?} has an empty label"));
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
        // A wildcard has to leave a registrable name behind it. `*.com` reads
        // like an ordinary entry and grants every host in a whole top-level
        // domain, which is exactly the pattern a one-click "Allow host" on a
        // denial the sandbox chose the text of would produce.
        if literal.len() != lower.len() && !literal.contains('.') {
            return Err(format!(
                "host pattern {s:?} wildcards a whole top-level domain; use at least *.example.com"
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

    /// The literal IP address this entry *is*, if it is one rather than a name.
    ///
    /// Either family: an IPv6 entry is stored in the canonical spelling
    /// [`HostPattern::parse`] gave it, which parses back to the same address.
    /// The proxy uses this to tell "the user allowed `127.0.0.1`" from "a name
    /// the repository allowed happens to resolve there".
    pub fn as_ip(&self) -> Option<std::net::IpAddr> {
        // A wildcard is never an address, and `"1.2.3.4".parse()` would not
        // see the `*.` in front of it anyway; checked so the intent is plain.
        if self.0.starts_with("*.") {
            return None;
        }
        self.0.parse().ok()
    }
}

/// A host as it is compared and as it is published: lowercased, with the
/// trailing dot of a fully qualified name dropped, so `github.com.` and
/// `github.com` are one host; an IPv6 literal without its URI brackets and in
/// canonical spelling, so `[2606:4700:0:0::1111]` and `2606:4700::1111` are one
/// address.
///
/// Ports are deliberately *not* stripped here. Splitting `host:port` is the
/// proxy's job, where an IPv6 literal (`[::1]:8080`) still has to be told from
/// a bare address; a pattern carrying a port is rejected at parse time instead.
pub fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    let h = h.strip_suffix('.').unwrap_or(&h);
    match ipv6_literal(h) {
        Some(v6) => v6.to_string(),
        None => h.to_string(),
    }
}

/// `text` as the IPv6 address it spells, bracketed or bare, or `None` for
/// anything else - including a bracketed thing that is not an address, and
/// `[v6]:port`, whose brackets do not enclose the whole of it.
fn ipv6_literal(text: &str) -> Option<Ipv6Addr> {
    let bare = text
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(text);
    bare.parse().ok()
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

    /// Whether some entry is the literal address `addr`.
    ///
    /// This is what permits a destination the proxy otherwise refuses as
    /// private: allowing `127.0.0.1` by writing that address down is a
    /// deliberate act, whereas a *name* the repository added resolving to
    /// loopback is the attack the refusal exists for. See
    /// [`crate::net::proxy`].
    pub fn allows_literal_addr(&self, addr: &std::net::IpAddr) -> bool {
        self.patterns
            .iter()
            .any(|p| p.as_ip().as_ref() == Some(addr))
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
        for bad in [
            "",
            " ",
            "*",
            "foo.*",
            "a/b",
            "host:80",
            "*.",
            "*bar.com",
            "*.com",
            "*.localhost",
        ] {
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

    /// A wildcard must leave a registrable name behind it: `*.com` would hand
    /// the sandbox every `.com` host from one click on a denial toast whose
    /// text the sandbox chose.
    #[test]
    fn a_wildcard_needs_more_than_a_top_level_domain_behind_it() {
        for bad in ["*.com", "*.CO", "*.internal"] {
            let err = HostPattern::parse(bad).unwrap_err();
            assert!(err.contains("top-level domain"), "{bad}: {err}");
        }
        for ok in ["*.example.com", "*.a.b.c", "*.co.uk"] {
            assert!(HostPattern::parse(ok).is_ok(), "{ok}");
        }
        // A bare name with no dot is still an ordinary exact entry: an
        // intranet host called `build` is a host, not a wildcard.
        assert!(HostPattern::parse("build").is_ok());
    }

    /// The proxy needs to tell an entry that *is* an address from one that
    /// merely resolves to one; only the former may reach a private network.
    #[test]
    fn a_literal_address_entry_is_recognised_as_one() {
        assert_eq!(
            HostPattern::parse("127.0.0.1").unwrap().as_ip(),
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(HostPattern::parse("localhost").unwrap().as_ip(), None);
        assert_eq!(HostPattern::parse("*.example.com").unwrap().as_ip(), None);
        let list = Allowlist::from_strings(&["127.0.0.1".to_string(), "localhost".to_string()]);
        assert!(list.allows_literal_addr(&"127.0.0.1".parse().unwrap()));
        // The name resolving there is not the same permission.
        assert!(!list.allows_literal_addr(&"::1".parse().unwrap()));
        assert!(!list.allows_literal_addr(&"10.0.0.5".parse().unwrap()));
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

    /// A fully qualified name is the same host with or without its root dot,
    /// whichever side of the comparison the dot is on.
    #[test]
    fn a_trailing_dot_is_the_same_host_on_either_side() {
        let dotted = HostPattern::parse("example.com.").unwrap();
        assert_eq!(dotted.as_str(), "example.com");
        assert!(dotted.matches("example.com"));
        assert!(dotted.matches("example.com."));
        let plain = HostPattern::parse("example.com").unwrap();
        assert!(plain.matches("example.com."));
        // A wildcard written fully qualified still covers its subdomains.
        let w = HostPattern::parse("*.example.com.").unwrap();
        assert!(w.matches("a.example.com"));
        assert!(w.matches("a.example.com."));
        // One root dot is the root label; two are an empty label, which is
        // never a host, and a bare dot is nothing at all.
        for bad in ["example.com..", "a..b", "..", ".", "*.."] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// An IPv6 address is written with colons, which every other entry is
    /// refused for, and in a URI it wears brackets that are not part of the
    /// address. Both spellings are the one literal.
    #[test]
    fn an_ipv6_literal_is_accepted_with_or_without_brackets() {
        let addr: std::net::IpAddr = "2606:4700::1111".parse().unwrap();
        for spelled in [
            "2606:4700::1111",
            "[2606:4700::1111]",
            "2606:4700:0:0::1111",
        ] {
            let p = HostPattern::parse(spelled).unwrap_or_else(|e| panic!("{spelled}: {e}"));
            assert_eq!(p.as_str(), "2606:4700::1111", "{spelled}");
            assert_eq!(p.as_ip(), Some(addr), "{spelled}");
            // The proxy hands the host over bare, the way `CONNECT [v6]:443`
            // yields it, and it may also arrive still wearing its brackets.
            assert!(p.matches("2606:4700::1111"), "{spelled}");
            assert!(p.matches("[2606:4700::1111]"), "{spelled}");
            assert!(p.matches("2606:4700:0:0:0:0:0:1111"), "{spelled}");
            assert!(!p.matches("2606:4700::1112"), "{spelled}");
        }
        let list = Allowlist::from_strings(&["[2606:4700::1111]".to_string()]);
        assert!(list.allows("2606:4700::1111"));
        assert!(list.allows_literal_addr(&addr));
        assert_eq!(list.to_strings(), vec!["2606:4700::1111".to_string()]);
        // A colon that is not an IPv6 address is still a port, and refused; a
        // bracketed thing that is not an address is not a host either.
        for bad in ["host:80", "[::1]:8080", "[example.com]", "[", "[]", "::1]"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?}");
        }
    }
}
