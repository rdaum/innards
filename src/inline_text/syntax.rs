use std::path::PathBuf;

use anyhow::{Result, anyhow};
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

pub(super) struct SyntaxHighlighter {
    pub(super) syntax_set: SyntaxSet,
    pub(super) theme: Theme,
    syntax_name: String,
}

impl SyntaxHighlighter {
    pub(super) fn new(path: &PathBuf) -> Result<Self> {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .or_else(|| theme_set.themes.values().next())
            .cloned()
            .ok_or_else(|| anyhow!("no syntect themes available"))?;
        let syntax = syntax_set
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
        let syntax_name = syntax.name.clone();

        Ok(Self {
            syntax_set,
            theme,
            syntax_name,
        })
    }

    pub(super) fn syntax(&self) -> &SyntaxReference {
        self.syntax_set
            .find_syntax_by_name(&self.syntax_name)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }
}
