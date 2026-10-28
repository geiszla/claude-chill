use clap::Parser;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");

#[derive(Parser, Debug)]
#[command(
    name = "claude-chill",
    version = VERSION,
    about = "A PTY proxy that tames Claude Code's massive terminal updates"
)]
pub struct Cli {
    /// Command to run (e.g., "claude")
    pub command: String,

    /// Arguments to pass to the command
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,

    /// Max lines stored for lookback (default: 100000)
    #[arg(short = 'H', long = "history")]
    pub history_lines: Option<usize>,

    /// Key to toggle lookback mode, quote to prevent glob expansion (default: "[ctrl][6]")
    #[arg(short = 'k', long = "lookback-key")]
    pub lookback_key: Option<String>,

    /// Auto-lookback timeout in ms, 0 to disable (default: 15000)
    #[arg(short = 'a', long = "auto-lookback-timeout")]
    pub auto_lookback_timeout: Option<u64>,

    /// Rewrite the terminal scrollback from the emulator this many ms after a resize settles, 0 to disable (default: 500)
    #[arg(short = 'r', long = "resize-refresh")]
    pub resize_refresh: Option<u64>,

    /// Once after startup, rewrite the terminal scrollback from the emulator when its history has grown and then stayed unchanged this many ms, 0 to disable (default: 1000)
    #[arg(short = 's', long = "startup-refresh")]
    pub startup_refresh: Option<u64>,
}
