//! Linux doctor checks that compile without a GNOME Shell extension or the
//! deleted `dbus` crate. IBus/IME is not a v1 hard dependency — PTY + the
//! edit buffer drive completions — so those checks stay out.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use fig_os_shim::Context;
use fig_util::system_info::linux::{DisplayServer, SandboxKind, detect_sandbox, get_display_server};

use crate::cli::doctor::{DoctorCheck, DoctorCheckType, DoctorError, Platform, doctor_error, doctor_warning};

pub struct DisplayServerCheck;

#[async_trait]
impl DoctorCheck<Arc<Context>> for DisplayServerCheck {
    fn name(&self) -> Cow<'static, str> {
        "Display Server Check".into()
    }

    async fn get_type(&self, _: &Arc<Context>, _: Platform) -> DoctorCheckType {
        DoctorCheckType::NormalCheck
    }

    async fn check(&self, ctx: &Arc<Context>) -> Result<(), DoctorError> {
        match get_display_server(ctx) {
            Ok(DisplayServer::X11 | DisplayServer::Wayland) => Ok(()),
            Err(fig_util::Error::UnknownDisplayServer(server)) => Err(doctor_error!(
                "Unknown value set for XDG_SESSION_TYPE: {}. This must be set to x11 or wayland.",
                server
            )),
            Err(err) => Err(doctor_error!(
                "Unknown error occurred when detecting the display server: {:?}. Is XDG_SESSION_TYPE set to x11 or wayland?",
                err
            )),
        }
    }
}

pub struct SandboxCheck;

#[async_trait]
impl DoctorCheck<Arc<Context>> for SandboxCheck {
    fn name(&self) -> Cow<'static, str> {
        "App is not running in a sandbox".into()
    }

    async fn get_type(&self, _: &Arc<Context>, _: Platform) -> DoctorCheckType {
        DoctorCheckType::NormalCheck
    }

    async fn check(&self, _: &Arc<Context>) -> Result<(), DoctorError> {
        match detect_sandbox() {
            SandboxKind::None => Ok(()),
            SandboxKind::Flatpak => Err(doctor_error!("Running under Flatpak is not supported.")),
            SandboxKind::Snap => Err(doctor_error!("Running under Snap is not supported.")),
            SandboxKind::Docker => Err(doctor_warning!(
                "Support for Docker is in development. It may not work properly on your system."
            )),
            SandboxKind::Container(Some(engine)) => {
                Err(doctor_error!("Running under `{engine}` containers is not supported."))
            },
            SandboxKind::Container(None) => Err(doctor_error!("Running under non-docker containers is not supported.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use fig_os_shim::Env;

    use super::*;

    #[tokio::test]
    async fn display_server_accepts_x11_and_wayland() {
        for session in ["x11", "wayland"] {
            let ctx = Context::builder()
                .with_env(Env::from_slice(&[("XDG_SESSION_TYPE", session)]))
                .build_fake();
            assert!(
                DisplayServerCheck.check(&ctx).await.is_ok(),
                "{session} must be a supported display server"
            );
        }
    }

    #[tokio::test]
    async fn display_server_rejects_unknown_session_type() {
        let ctx = Context::builder()
            .with_env(Env::from_slice(&[("XDG_SESSION_TYPE", "mir")]))
            .build_fake();
        let err = DisplayServerCheck.check(&ctx).await.expect_err("mir");
        assert!(err.to_string().contains("XDG_SESSION_TYPE"), "{err:?}");
    }

    #[test]
    fn linux_doctor_does_not_require_a_gnome_shell_extension() {
        let production = include_str!("linux.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains("dbus::gnome_shell") && !production.contains("GnomeExtension"),
            "Linux v1 caret is AT-SPI / IBus, not a GNOME Shell extension"
        );
        assert!(
            !production.contains("IBusEnvCheck")
                && !production.contains("IBusRunningCheck")
                && !production.contains("IBusConnectionCheck"),
            "IBus is not a v1 hard dependency; PTY + edit buffer drive completions"
        );
    }
}
