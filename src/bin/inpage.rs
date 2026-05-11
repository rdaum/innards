use anyhow::Result;
use innards::inline_text::{Mode, run};

fn main() -> Result<()> {
    run(Mode::View)
}
