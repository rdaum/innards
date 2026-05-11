use std::io::{self, Stdout, Write};

use anyhow::Result;
use crossterm::ExecutableCommand;
use crossterm::cursor::{MoveTo, Show};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::MIN_HEIGHT;

pub(super) struct TerminalGuard {
    pub(super) terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(super) mode: TerminalMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalMode {
    Inline,
    Fullscreen,
}

impl TerminalGuard {
    pub(super) fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let terminal = Self::new_inline_terminal(height)?;
        Ok(Self {
            terminal,
            mode: TerminalMode::Inline,
        })
    }

    pub(super) fn new_inline_terminal(height: u16) -> Result<Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(MIN_HEIGHT)),
            },
        )?;
        Ok(terminal)
    }

    pub(super) fn new_fullscreen_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;
        Ok(terminal)
    }

    pub(super) fn resize(&mut self, height: u16, anchor_y: u16) -> Result<()> {
        debug_assert_eq!(self.mode, TerminalMode::Inline);
        self.terminal.clear()?;
        io::stdout().execute(MoveTo(0, anchor_y))?;
        self.terminal = Self::new_inline_terminal(height)?;
        Ok(())
    }

    pub(super) fn enter_fullscreen(&mut self) -> Result<()> {
        if self.mode == TerminalMode::Fullscreen {
            return Ok(());
        }
        self.terminal.clear()?;
        io::stdout().execute(EnterAlternateScreen)?;
        self.terminal = Self::new_fullscreen_terminal()?;
        self.terminal.clear()?;
        self.mode = TerminalMode::Fullscreen;
        Ok(())
    }

    pub(super) fn leave_fullscreen(&mut self, height: u16) -> Result<()> {
        if self.mode == TerminalMode::Inline {
            return Ok(());
        }
        self.terminal.clear()?;
        io::stdout().execute(LeaveAlternateScreen)?;
        self.terminal = Self::new_inline_terminal(height)?;
        self.terminal.clear()?;
        self.mode = TerminalMode::Inline;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        if self.mode == TerminalMode::Fullscreen {
            let _ = io::stdout().execute(LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        let _ = io::stdout().execute(Show);
        let _ = io::stdout().flush();
    }
}
