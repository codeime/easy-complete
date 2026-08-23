use fig_os_shim::Context;
use tokio::sync::mpsc::Sender;

use crate::index::UpdatePackage;
use crate::{Error, UpdateStatus};

#[allow(dead_code)]
pub(crate) async fn update(
    _package: UpdatePackage,
    _tx: Sender<UpdateStatus>,
    _interactive: bool,
    _relaunch_dashboard: bool,
) -> Result<(), Error> {
    Err(Error::UpdateFailed(
        crate::update_os_policy::WINDOWS_UPDATER_UNAVAILABLE.into(),
    ))
}

pub(crate) async fn uninstall_desktop(_ctx: &Context) -> Result<(), Error> {
    let _ = fig_integrations::launch_at_login::set_enabled(false).await;
    if let Ok(dir) = fig_util::directories::fig_data_dir() {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    Ok(())
}
