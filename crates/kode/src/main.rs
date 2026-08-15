mod doctor;
mod exec;
mod pipeline;
mod remember;
mod setup;
mod status;
mod tui;
mod verify;

use clap::Parser;
use kode_core::{CancellationToken, cancel_on_ctrl_c};

#[derive(Parser)]
#[command(name = "kode", version, about = "Kode: a local-first coding agent")]
struct Cli {
    /// Increase log verbosity (-v info, -vv debug).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Subcommand to run. When omitted, Kode launches the interactive TUI.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Show Kode's current status.
    Status,
    /// Run an agentic task against the configured model.
    Exec {
        /// The task to accomplish.
        task: String,
    },
    /// Detect the project and run its verification pipeline.
    Verify,
    /// Run diagnostic checks across config, LLM, zindeks, Ingat, git, and env.
    Doctor,
    /// Install/bootstrap the zindeks and Ingat engines (consent-gated).
    Setup {
        /// Skip confirmation prompts and proceed with all installs.
        #[arg(long)]
        yes: bool,
    },
    /// Save an explicit engineering memory to Ingat.
    Remember {
        /// The memory text.
        text: String,
        /// Memory kind: project-rule, architecture-decision, convention,
        /// known-issue, build-knowledge, rejected-approach,
        /// user-preference, historical-solution.
        #[arg(long, default_value = "project-rule")]
        kind: String,
        /// Tag to attach; may be repeated.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
}

fn init_tracing(verbose: u8) {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::EnvFilter::from_default_env()
    } else {
        let level = match verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        };
        tracing_subscriber::EnvFilter::new(level)
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let token = CancellationToken::new();
    cancel_on_ctrl_c(token.clone());

    match cli.command {
        None => {
            let cwd = std::env::current_dir()?;
            tui::run(&cwd, token).await?;
        }
        Some(Command::Status) => {
            let cwd = std::env::current_dir()?;
            status::run(&cwd).await?;
        }
        Some(Command::Exec { task }) => {
            let cwd = std::env::current_dir()?;
            exec::run(&task, &cwd, token).await?;
        }
        Some(Command::Setup { yes }) => {
            let cwd = std::env::current_dir()?;
            setup::run(yes, &cwd).await?;
        }
        Some(Command::Verify) => {
            let cwd = std::env::current_dir()?;
            verify::run(&cwd, token).await?;
        }
        Some(Command::Doctor) => {
            let cwd = std::env::current_dir()?;
            doctor::run(&cwd).await?;
        }
        Some(Command::Remember { text, kind, tags }) => {
            let cwd = std::env::current_dir()?;
            remember::run(&text, &kind, tags, &cwd).await?;
        }
    }

    Ok(())
}
