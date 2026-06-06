use std::sync::Arc;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::git::GitoliteAdmin;

pub struct HighlighterState {
    pub syntax_set: SyntaxSet,
    pub light_theme: Theme,
    pub dark_theme: Theme,
}

impl HighlighterState {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();

        let light_theme = theme_set.themes["InspiredGitHub"].clone();
        let dark_theme = theme_set.themes["base16-ocean.dark"].clone();

        for syntax in syntax_set.syntaxes() {
            tracing::info!("Syntax: {} -> {:?}", syntax.name, syntax.file_extensions);
        }

        Self {
            syntax_set,
            light_theme,
            dark_theme,
        }
    }

    /// Highlight a single line of content for both themes.
    /// Returns (light_html, dark_html).
    /// `extension` is the file extension without the dot, e.g. "rs", "ts", "py".
    pub fn highlight_line(&self, content: &str, extension: &str) -> (String, String) {
        let syntax = self
            .syntax_set
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let line = if content.ends_with('\n') {
            content.to_string()
        } else {
            format!("{content}\n")
        };

        let light = self.highlight_with_theme(&line, syntax, &self.light_theme);
        let dark = self.highlight_with_theme(&line, syntax, &self.dark_theme);

        (light, dark)
    }

    fn highlight_with_theme(
        &self,
        line: &str,
        syntax: &syntect::parsing::SyntaxReference,
        theme: &Theme,
    ) -> String {
        use syntect::easy::HighlightLines;
        use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};

        let mut h = HighlightLines::new(syntax, theme);
        match h.highlight_line(line, &self.syntax_set) {
            Ok(ranges) => {
                // ranges is Vec<(Style, &str)> — pass as slice
                styled_line_to_highlighted_html(&ranges[..], IncludeBackground::No)
                    .unwrap_or_else(|_| Self::escape_html(line))
            }
            Err(_) => Self::escape_html(line),
        }
    }

    fn escape_html(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

/// Shared across all Warp filters via Arc<AppStateInner>.
#[derive(Clone)]
pub struct AppState(pub Arc<AppStateInner>);

pub struct AppStateInner {
    pub config: Config,
    /// The gitolite-admin repo handle — serialised behind a Mutex so only one
    /// coroutine writes/pushes at a time. Redis is used for cross-process
    /// locking (in case we ever scale the sidecar), but the Mutex gives us a
    /// fast in-process guard.
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