use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use ec_engine::{CompleteRequest, EngineClient, default_specs_dir};
use eyre::Result;

/// Run the headless completion engine
#[derive(Debug, PartialEq, Args)]
pub struct EngineArgs {
    #[command(subcommand)]
    command: EngineCommand,
}

#[derive(Debug, PartialEq, clap::Subcommand)]
enum EngineCommand {
    /// Print suggestions for a command buffer
    Complete {
        /// Shell buffer to complete, e.g. "git ch"
        #[arg(long)]
        buffer: String,
        /// Working directory used to resolve local specs
        #[arg(long)]
        cwd: Option<String>,
        /// Override the bundled specs directory
        #[arg(long)]
        specs_dir: Option<PathBuf>,
    },
}

impl EngineArgs {
    pub async fn execute(self) -> Result<ExitCode> {
        match self.command {
            EngineCommand::Complete { buffer, cwd, specs_dir } => {
                let specs_dir = specs_dir.unwrap_or_else(default_specs_dir);
                let engine = EngineClient::spawn(specs_dir).map_err(|err| eyre::eyre!("{err}"))?;
                let result = engine
                    .complete(CompleteRequest {
                        buffer,
                        cwd: cwd.unwrap_or_else(|| {
                            std::env::current_dir().map_or_else(|_err| "/".into(), |p| p.display().to_string())
                        }),
                        cursor: None,
                        ..CompleteRequest::default()
                    })
                    .await
                    .map_err(|err| eyre::eyre!("{err}"))?;
                // Keep this diagnostic command lossless. Insertion metadata,
                // the normalized match term, and current-argument context are
                // precisely the fields needed when comparing the native
                // engine with the former WebView implementation.
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(ExitCode::SUCCESS)
            },
        }
    }
}
