use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

const CONFIG_DIR_NAME: &str = "innards";
const CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Default)]
pub struct InnardsConfig {
    pub inmacs: InmacsConfig,
    pub keybindings: ConfigKeybindings,
}

#[derive(Debug, Clone, Default)]
pub struct InmacsConfig {
    pub fill_column: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigKeybindings {
    pub inline: HashMap<String, BindingList>,
    pub navsplat: HashMap<String, BindingList>,
    pub rebase: HashMap<String, BindingList>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum BindingList {
    One(String),
    Many(Vec<String>),
}

impl BindingList {
    fn values(&self) -> Vec<&str> {
        match self {
            Self::One(value) => vec![value.as_str()],
            Self::Many(values) => values.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    inmacs: RawInmacsConfig,
    #[serde(default)]
    keybindings: RawKeybindings,
}

#[derive(Debug, Deserialize, Default)]
struct RawInmacsConfig {
    fill_column: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RawKeybindings {
    #[serde(default)]
    inline: HashMap<String, BindingList>,
    #[serde(default)]
    navsplat: HashMap<String, BindingList>,
    #[serde(default)]
    rebase: HashMap<String, BindingList>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct KeyPress {
    code: KeyCode,
    modifiers: KeyModifiers,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct KeySequence(Vec<KeyPress>);

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<(String, KeySequence)>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum KeymapMatch {
    None,
    Prefix,
    Action(String),
}

impl InnardsConfig {
    pub fn load() -> Result<Self> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }

        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let raw: RawConfig = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        Ok(Self {
            inmacs: InmacsConfig {
                fill_column: raw.inmacs.fill_column,
            },
            keybindings: ConfigKeybindings {
                inline: raw.keybindings.inline,
                navsplat: raw.keybindings.navsplat,
                rebase: raw.keybindings.rebase,
            },
        })
    }
}

pub fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME))
}

impl Keymap {
    pub fn from_defaults(defaults: &[(&str, &[&str])]) -> Result<Self> {
        let mut keymap = Self {
            bindings: Vec::new(),
        };
        for (action, bindings) in defaults {
            keymap.set_action(action, bindings.iter().copied())?;
        }
        Ok(keymap)
    }

    pub fn apply_overrides(&mut self, overrides: &HashMap<String, BindingList>) -> Result<()> {
        for (action, bindings) in overrides {
            self.set_action(action, bindings.values())?;
        }
        Ok(())
    }

    pub fn match_key(&self, pending: &[KeyPress], key: &KeyEvent) -> KeymapMatch {
        self.match_key_for_actions(&[], pending, key)
    }

    pub fn match_key_for_actions(
        &self,
        actions: &[&str],
        pending: &[KeyPress],
        key: &KeyEvent,
    ) -> KeymapMatch {
        let Some(key) = KeyPress::from_event(key) else {
            return KeymapMatch::None;
        };
        let mut candidate = pending.to_vec();
        candidate.push(key);

        let mut is_prefix = false;
        for (action, sequence) in &self.bindings {
            if !actions.is_empty() && !actions.contains(&action.as_str()) {
                continue;
            }
            if sequence.0 == candidate {
                return KeymapMatch::Action(action.clone());
            }
            if sequence.0.starts_with(&candidate) {
                is_prefix = true;
            }
        }

        if is_prefix {
            KeymapMatch::Prefix
        } else {
            KeymapMatch::None
        }
    }

    pub fn keypress_from_event(&self, key: &KeyEvent) -> Option<KeyPress> {
        KeyPress::from_event(key)
    }

    fn set_action<'a>(
        &mut self,
        action: &str,
        bindings: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        self.bindings.retain(|(existing, _)| existing != action);
        for binding in bindings {
            let sequence = KeySequence::parse(binding)
                .with_context(|| format!("invalid binding for action `{action}`"))?;
            self.bindings.push((action.to_string(), sequence));
        }
        Ok(())
    }
}

impl KeySequence {
    fn parse(input: &str) -> Result<Self> {
        let keys = input
            .split_whitespace()
            .map(KeyPress::parse)
            .collect::<Result<Vec<_>>>()?;
        if keys.is_empty() {
            return Err(anyhow!("empty key sequence"));
        }
        Ok(Self(keys))
    }
}

impl KeyPress {
    fn from_event(event: &KeyEvent) -> Option<Self> {
        let mut code = event.code;
        let mut modifiers = event.modifiers;

        if let KeyCode::Char(ch) = code {
            code = KeyCode::Char(ch.to_ascii_lowercase());
            modifiers.remove(KeyModifiers::SHIFT);
        }

        Some(Self { code, modifiers })
    }

    fn parse(input: &str) -> Result<Self> {
        let mut rest = input.trim().to_ascii_lowercase();
        if rest.is_empty() {
            return Err(anyhow!("empty key"));
        }
        rest = rest.replace('+', "-");

        let mut modifiers = KeyModifiers::empty();
        loop {
            if let Some(value) = rest.strip_prefix("ctrl-") {
                modifiers.insert(KeyModifiers::CONTROL);
                rest = value.to_string();
            } else if let Some(value) = rest.strip_prefix("control-") {
                modifiers.insert(KeyModifiers::CONTROL);
                rest = value.to_string();
            } else if let Some(value) = rest.strip_prefix("alt-") {
                modifiers.insert(KeyModifiers::ALT);
                rest = value.to_string();
            } else if let Some(value) = rest.strip_prefix("meta-") {
                modifiers.insert(KeyModifiers::ALT);
                rest = value.to_string();
            } else if let Some(value) = rest.strip_prefix("shift-") {
                modifiers.insert(KeyModifiers::SHIFT);
                rest = value.to_string();
            } else {
                break;
            }
        }

        let code = match rest.as_str() {
            "esc" | "escape" => KeyCode::Esc,
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" | "bs" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "pageup" | "page-up" | "pgup" => KeyCode::PageUp,
            "pagedown" | "page-down" | "pgdn" => KeyCode::PageDown,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "space" => KeyCode::Char(' '),
            "null" => KeyCode::Null,
            value if value.chars().count() == 1 => {
                KeyCode::Char(value.chars().next().expect("checked char count"))
            }
            _ => return Err(anyhow!("unknown key `{input}`")),
        };

        Ok(Self { code, modifiers })
    }
}

impl fmt::Display for KeyPress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(f, "ctrl-")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            write!(f, "alt-")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            write!(f, "shift-")?;
        }
        match self.code {
            KeyCode::Char(' ') => write!(f, "space"),
            KeyCode::Char(ch) => write!(f, "{ch}"),
            KeyCode::Esc => write!(f, "esc"),
            KeyCode::Enter => write!(f, "enter"),
            KeyCode::Tab => write!(f, "tab"),
            KeyCode::Backspace => write!(f, "backspace"),
            KeyCode::Delete => write!(f, "delete"),
            KeyCode::Up => write!(f, "up"),
            KeyCode::Down => write!(f, "down"),
            KeyCode::Left => write!(f, "left"),
            KeyCode::Right => write!(f, "right"),
            KeyCode::PageUp => write!(f, "pageup"),
            KeyCode::PageDown => write!(f, "pagedown"),
            KeyCode::Home => write!(f, "home"),
            KeyCode::End => write!(f, "end"),
            KeyCode::Null => write!(f, "null"),
            _ => write!(f, "{:?}", self.code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ctrl_x_chord() {
        let sequence = KeySequence::parse("ctrl-x ctrl-s").unwrap();

        assert_eq!(sequence.0.len(), 2);
        assert_eq!(sequence.0[0].code, KeyCode::Char('x'));
        assert!(sequence.0[0].modifiers.contains(KeyModifiers::CONTROL));
        assert_eq!(sequence.0[1].code, KeyCode::Char('s'));
        assert!(sequence.0[1].modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn matches_prefix_then_action() {
        let keymap = Keymap::from_defaults(&[("save", &["ctrl-x ctrl-s"])]).unwrap();
        let first = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let second = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let pending = vec![keymap.keypress_from_event(&first).unwrap()];

        assert_eq!(keymap.match_key(&[], &first), KeymapMatch::Prefix);
        assert_eq!(
            keymap.match_key(&pending, &second),
            KeymapMatch::Action("save".to_string())
        );
    }

    #[test]
    fn parses_config_file_shape() {
        let raw: RawConfig = toml::from_str(
            r#"
            [inmacs]
            fill_column = 100

            [keybindings.inline]
            save = "ctrl-x ctrl-s"
            page_up = ["alt-v", "pageup"]

            [keybindings.navsplat]
            open = "enter"

            [keybindings.rebase]
            save = "ctrl-x ctrl-s"
            move_up = "alt-p"
            "#,
        )
        .unwrap();

        assert_eq!(raw.inmacs.fill_column, Some(100));
        assert!(raw.keybindings.inline.contains_key("save"));
        assert!(raw.keybindings.inline.contains_key("page_up"));
        assert!(raw.keybindings.navsplat.contains_key("open"));
        assert!(raw.keybindings.rebase.contains_key("save"));
        assert!(raw.keybindings.rebase.contains_key("move_up"));
    }
}
