//! Grammar registry: file extension -> language -> a configured tree-sitter
//! highlight configuration, built once per language on first use.

use crate::highlight::theme::STYLE_NAMES;
use std::sync::OnceLock;
use tree_sitter_highlight::HighlightConfiguration;

/// A language BondSymphonic bundles a grammar for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Json,
    Toml,
    Yaml,
    Html,
    Css,
    Markdown,
    Bash,
    C,
    Cpp,
    Go,
}

/// Every language, in the order their configurations are cached.
const ALL: [Language; 15] = [
    Language::Rust,
    Language::JavaScript,
    Language::TypeScript,
    Language::Tsx,
    Language::Python,
    Language::Json,
    Language::Toml,
    Language::Yaml,
    Language::Html,
    Language::Css,
    Language::Markdown,
    Language::Bash,
    Language::C,
    Language::Cpp,
    Language::Go,
];

impl Language {
    /// The language for a path, by file extension. `None` for extensionless
    /// names and for extensions no bundled grammar covers.
    pub fn from_path(path: &str) -> Option<Language> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "py" | "pyi" => Language::Python,
            "json" | "jsonc" => Language::Json,
            "toml" => Language::Toml,
            "yaml" | "yml" => Language::Yaml,
            "html" | "htm" => Language::Html,
            "css" => Language::Css,
            "md" | "markdown" => Language::Markdown,
            "sh" | "bash" | "zsh" => Language::Bash,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "go" => Language::Go,
            _ => return None,
        })
    }

    /// The lowercase identifier used in queries and in the UI.
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Python => "python",
            Language::Json => "json",
            Language::Toml => "toml",
            Language::Yaml => "yaml",
            Language::Html => "html",
            Language::Css => "css",
            Language::Markdown => "markdown",
            Language::Bash => "bash",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Go => "go",
        }
    }

    /// The configured highlighter for this language, built on first use.
    pub fn config(self) -> &'static HighlightConfiguration {
        static CONFIGS: OnceLock<Vec<OnceLock<HighlightConfiguration>>> = OnceLock::new();
        let slots = CONFIGS.get_or_init(|| ALL.iter().map(|_| OnceLock::new()).collect());
        let idx = ALL
            .iter()
            .position(|l| *l == self)
            .expect("language listed in ALL");
        slots[idx].get_or_init(|| {
            let mut cfg = build(self).expect("bundled grammar and query compile");
            cfg.configure(&STYLE_NAMES);
            cfg
        })
    }
}

/// TypeScript's own highlights query only covers what TypeScript adds to
/// JavaScript, so the two are concatenated for `.ts` and `.tsx`.
fn typescript_highlights() -> &'static str {
    static QUERY: OnceLock<String> = OnceLock::new();
    QUERY.get_or_init(|| {
        format!(
            "{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY
        )
    })
}

/// Likewise, C++'s highlights query only covers what C++ adds to C; without
/// C's query even `int main() { return 0; }` comes back unhighlighted.
fn cpp_highlights() -> &'static str {
    static QUERY: OnceLock<String> = OnceLock::new();
    QUERY.get_or_init(|| {
        format!(
            "{}\n{}",
            tree_sitter_c::HIGHLIGHT_QUERY,
            tree_sitter_cpp::HIGHLIGHT_QUERY
        )
    })
}

fn build(lang: Language) -> Result<HighlightConfiguration, tree_sitter::QueryError> {
    // (language, highlights query, injections query, locals query)
    let (language, highlights, injections, locals): (tree_sitter::Language, &str, &str, &str) =
        match lang {
            Language::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
                tree_sitter_rust::INJECTIONS_QUERY,
                "",
            ),
            Language::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::INJECTIONS_QUERY,
                tree_sitter_javascript::LOCALS_QUERY,
            ),
            Language::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                typescript_highlights(),
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            ),
            Language::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                typescript_highlights(),
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            ),
            Language::Python => (
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Language::Json => (
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Language::Toml => (
                tree_sitter_toml_ng::LANGUAGE.into(),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Language::Yaml => (
                tree_sitter_yaml::LANGUAGE.into(),
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Language::Html => (
                tree_sitter_html::LANGUAGE.into(),
                tree_sitter_html::HIGHLIGHTS_QUERY,
                tree_sitter_html::INJECTIONS_QUERY,
                "",
            ),
            Language::Css => (
                tree_sitter_css::LANGUAGE.into(),
                tree_sitter_css::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Language::Markdown => (
                tree_sitter_md::LANGUAGE.into(),
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
                tree_sitter_md::INJECTION_QUERY_BLOCK,
                "",
            ),
            Language::Bash => (
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Language::C => (
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Language::Cpp => (tree_sitter_cpp::LANGUAGE.into(), cpp_highlights(), "", ""),
            Language::Go => (
                tree_sitter_go::LANGUAGE.into(),
                tree_sitter_go::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
        };
    HighlightConfiguration::new(language, lang.name(), highlights, injections, locals)
}
