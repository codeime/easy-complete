use std::process::ExitCode;

use clap::Subcommand;
use crossterm::style::Stylize;
use eyre::Result;
use serde_json::json;

use super::OutputFormat;

const TELEMETRY_ENABLED_KEY: &str = "telemetry.enabled";

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum TelemetrySubcommand {
    #[command(hide = true)]
    Enable,
    #[command(hide = true)]
    Disable,
    #[command(hide = true)]
    Status {
        /// Format of the output
        #[arg(long, short, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Send a single telemetry event (used by install/uninstall scripts)
    #[command(hide = true)]
    Track {
        /// Event name
        event: String,
    },
}

impl TelemetrySubcommand {
    pub async fn execute(&self) -> Result<ExitCode> {
        match self {
            TelemetrySubcommand::Enable => {
                fastab_settings::settings::set_value(TELEMETRY_ENABLED_KEY, true)?;
                Ok(ExitCode::SUCCESS)
            },
            TelemetrySubcommand::Disable => {
                fastab_settings::settings::set_value(TELEMETRY_ENABLED_KEY, false)?;
                Ok(ExitCode::SUCCESS)
            },
            TelemetrySubcommand::Status { format } => {
                let status = fastab_settings::settings::get_bool_or(TELEMETRY_ENABLED_KEY, false);
                format.print(
                    || {
                        format!(
                            "Telemetry status: {}",
                            if status { "enabled" } else { "disabled" }.bold()
                        )
                    },
                    || {
                        json!({
                            TELEMETRY_ENABLED_KEY: status,
                        })
                    },
                );
                Ok(ExitCode::SUCCESS)
            },
            TelemetrySubcommand::Track { event } => {
                fastab_telemetry::track_blocking(event, json!({})).await;
                Ok(ExitCode::SUCCESS)
            },
        }
    }
}
