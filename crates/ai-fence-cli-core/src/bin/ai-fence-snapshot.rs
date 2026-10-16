//! Standalone local helper using the same implementation as ai-fence-cli snapshot.
use ai_fence_cli_core::workspace_snapshot::{execute, SnapshotAction};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Local Btrfs workspace checkpoints; no restore or workspace deletion")]
struct Cli {
    #[command(subcommand)]
    action: SnapshotAction,
}
fn main() -> anyhow::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&execute(Cli::parse().action)?)?
    );
    Ok(())
}
