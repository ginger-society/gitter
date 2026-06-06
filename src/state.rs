use std::sync::Arc;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet, SyntaxSetBuilder};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::git::GitoliteAdmin;

pub struct HighlighterState {
    /// Built-in syntaxes (Rust, Python, JS, CSS, HTML, …)
    default_set: SyntaxSet,
    /// Extra vendored syntaxes (TypeScript, TSX, …)
    extra_set: SyntaxSet,
    pub light_theme: Theme,
    pub dark_theme: Theme,
}

impl HighlighterState {
    pub fn new() -> Self {
        let default_set = SyntaxSet::load_defaults_newlines();

        // Load any .sublime-syntax files we've vendored under assets/syntaxes/.
        let mut builder = SyntaxSetBuilder::new();

        let extra_path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/syntaxes");
        match builder.add_from_folder(extra_path, true) {
            Ok(()) => tracing::info!("Loaded extra syntaxes from {extra_path}"),
            Err(e) => tracing::warn!("Extra syntaxes not loaded ({extra_path}): {e:#}"),
        }
        let extra_set = builder.build();

        let theme_set = ThemeSet::load_defaults();
        let light_theme = theme_set.themes["InspiredGitHub"].clone();
        let dark_theme  = theme_set.themes["base16-ocean.dark"].clone();

        // Log everything we have for diagnostics.
        for s in default_set.syntaxes() {
            tracing::debug!("default syntax: {} {:?}", s.name, s.file_extensions);
        }
        for s in extra_set.syntaxes() {
            tracing::info!("extra syntax:   {} {:?}", s.name, s.file_extensions);
        }

        Self { default_set, extra_set, light_theme, dark_theme }
    }

    /// Look up a syntax by extension, checking the extra (vendored) set first
    /// so that TS/TSX correctly resolve instead of falling back to plain text.
    fn find_syntax(&self, extension: &str) -> (&SyntaxReference, &SyntaxSet) {
        if let Some(s) = self.extra_set.find_syntax_by_extension(extension) {
            return (s, &self.extra_set);
        }
        let s = self
            .default_set
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.default_set.find_syntax_plain_text());
        (s, &self.default_set)
    }

    /// Highlight a single line for both themes. Returns (light_html, dark_html).
    /// `extension` is without the dot, e.g. "ts", "rs", "py".
    pub fn highlight_line(&self, content: &str, extension: &str) -> (String, String) {
        let (syntax, set) = self.find_syntax(extension);

        let line = if content.ends_with('\n') {
            content.to_string()
        } else {
            format!("{content}\n")
        };

        let light = Self::render(&line, syntax, set, &self.light_theme);
        let dark  = Self::render(&line, syntax, set, &self.dark_theme);
        (light, dark)
    }

    fn render(
        line: &str,
        syntax: &SyntaxReference,
        set: &SyntaxSet,
        theme: &Theme,
    ) -> String {
        use syntect::easy::HighlightLines;
        use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};

        let mut h = HighlightLines::new(syntax, theme);
        match h.highlight_line(line, set) {
            Ok(ranges) => styled_line_to_highlighted_html(&ranges[..], IncludeBackground::No)
                .unwrap_or_else(|_| Self::escape_html(line)),
            Err(_) => Self::escape_html(line),
        }
    }

    fn escape_html(s: &str) -> String {
        s.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
    }
}

// ── AppState (unchanged) ──────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState(pub Arc<AppStateInner>);

pub struct AppStateInner {
    pub config: Config,
    pub admin_repo: Mutex<GitoliteAdmin>,
    pub redis: redis::aio::ConnectionManager,
    pub highlighter: HighlighterState,
}

impl AppState {
    pub fn new(
        config: Config,
        admin_repo: GitoliteAdmin,
        redis: redis::aio::ConnectionManager,
    ) -> Self {
        Self(Arc::new(AppStateInner {
            config,
            admin_repo: Mutex::new(admin_repo),
            redis,
            highlighter: HighlighterState::new(),
        }))
    }
}