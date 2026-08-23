use std::process::ExitCode;

use clap::Args;
use eyre::{Result, bail};
use fig_ipc::local::update_command;
use fig_util::PRODUCT_NAME;

use crate::util::desktop::{LaunchArgs, desktop_app_running, launch_desktop};

#[derive(Debug, Args, PartialEq, Eq)]
pub struct UpdateArgs {}

impl UpdateArgs {
    pub async fn execute(self) -> Result<ExitCode> {
        if !cfg!(target_os = "macos") {
            bail!(
                "{}",
                fig_install::update_os_policy::cli_updates_only_on_macos(PRODUCT_NAME)
            );
        }

        if fig_util::system_info::is_remote() {
            bail!("Please check for {PRODUCT_NAME} updates from your host machine");
        }

        if !desktop_app_running() {
            launch_desktop(LaunchArgs {
                wait_for_socket: true,
                open_settings: false,
                immediate_update: true,
                verbose: false,
            })?;
        }

        println!("Checking for {PRODUCT_NAME} updates…");
        update_command(false).await?;
        Ok(ExitCode::SUCCESS)
    }
}
