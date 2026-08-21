use std::fmt::Display;

use anstream::adapter::strip_str;
use fig_integrations::Integration;
#[cfg(target_os = "linux")]
use fig_integrations::desktop_entry::{AutostartIntegration, DesktopEntryIntegration};
use fig_integrations::shell::ShellExt;
use fig_integrations::ssh::SshIntegration;
use fig_os_shim::{ContextArcProvider, ContextProvider, EnvProvider};
use fig_proto::fig::install_response::{InstallationStatus, Response};
use fig_proto::fig::result::Result as ProtoResultEnum;
use fig_proto::fig::server_originated_message::Submessage as ServerOriginatedSubMessage;
use fig_proto::fig::{InstallAction, InstallComponent, InstallRequest, InstallResponse, Result as ProtoResult};
use fig_settings::settings::SettingsProvider;
use fig_settings::state::StateProvider;
use fig_util::Shell;
#[cfg(target_os = "linux")]
use tracing::error;

use super::RequestResult;

#[allow(dead_code)]
async fn integration_status(integration: impl fig_integrations::Integration) -> ServerOriginatedSubMessage {
    ServerOriginatedSubMessage::InstallResponse(InstallResponse {
        response: Some(Response::InstallationStatus(match integration.is_installed().await {
            Ok(_) => InstallationStatus::Installed.into(),
            Err(_) => InstallationStatus::NotInstalled.into(),
        })),
    })
}

#[allow(dead_code)]
fn integration_unsupported() -> ServerOriginatedSubMessage {
    ServerOriginatedSubMessage::InstallResponse(InstallResponse {
        response: Some(Response::InstallationStatus(InstallationStatus::NotSupported.into())),
    })
}

fn integration_result(result: Result<(), impl Display>) -> ServerOriginatedSubMessage {
    ServerOriginatedSubMessage::InstallResponse(InstallResponse {
        response: Some(Response::Result(match result {
            Ok(()) => ProtoResult {
                result: ProtoResultEnum::Ok.into(),
                error: None,
            },
            Err(err) => ProtoResult {
                result: ProtoResultEnum::Error.into(),
                error: Some(err.to_string()),
            },
        })),
    })
}

pub async fn install<Ctx>(request: InstallRequest, ctx: &Ctx) -> RequestResult
where
    Ctx: SettingsProvider + StateProvider + ContextProvider + ContextArcProvider + Send + Sync,
{
    let response = match (request.component(), request.action()) {
        (InstallComponent::Dotfiles, action) => {
            let mut errs: Vec<String> = vec![];
            for shell in Shell::all() {
                match shell.get_shell_integrations(ctx.env()) {
                    Ok(integrations) => {
                        for integration in integrations {
                            let res = match action {
                                InstallAction::Install => integration.install().await,
                                InstallAction::Uninstall => integration.uninstall().await,
                                InstallAction::Status => integration.is_installed().await,
                            };

                            if let Err(err) = res {
                                errs.push(format!(
                                    "{integration}: {}",
                                    strip_str(&err.verbose_message().to_string())
                                ));
                            }
                        }
                    },
                    Err(err) => {
                        errs.push(format!("{shell}: {}", strip_str(&err.verbose_message().to_string())));
                    },
                }
            }

            match action {
                InstallAction::Install | InstallAction::Uninstall => integration_result(match &errs[..] {
                    [] => Ok(()),
                    errs => Err(errs.join("\n\n")),
                }),
                InstallAction::Status => ServerOriginatedSubMessage::InstallResponse(InstallResponse {
                    response: Some(Response::InstallationStatus(
                        if errs.is_empty() {
                            InstallationStatus::Installed
                        } else {
                            InstallationStatus::NotInstalled
                        }
                        .into(),
                    )),
                }),
            }
        },
        (InstallComponent::Ssh, action) => match SshIntegration::new() {
            Ok(ssh_integration) => match action {
                InstallAction::Install => integration_result(ssh_integration.install().await),
                InstallAction::Uninstall => integration_result(ssh_integration.uninstall().await),
                InstallAction::Status => integration_status(ssh_integration).await,
            },
            Err(err) => integration_result(Err(err)),
        },
        (InstallComponent::Ibus, _) => integration_result(Err("IBus install is legacy")),
        (InstallComponent::Accessibility, InstallAction::Install) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    use macos_utils::accessibility::{
                        accessibility_is_enabled,
                        open_accessibility,
                        prompt_for_accessibility,
                    };

                    if !accessibility_is_enabled() {
                        prompt_for_accessibility();
                        open_accessibility();
                    }

                    integration_result(Ok::<(), &str>(()))
                } else {
                    integration_result(Err("Accessibility permissions cannot be queried"))
                }
            }
        },
        (InstallComponent::Accessibility, InstallAction::Status) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    use macos_utils::accessibility::accessibility_is_enabled;

                    ServerOriginatedSubMessage::InstallResponse(InstallResponse {
                        response: Some(Response::InstallationStatus(if accessibility_is_enabled() {
                            InstallationStatus::Installed.into()
                        } else {
                            InstallationStatus::NotInstalled.into()
                        })),
                    })
                } else {
                    integration_unsupported()
                }
            }
        },
        (InstallComponent::Accessibility, InstallAction::Uninstall) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    integration_result(Ok::<(), &str>(()))
                } else {
                    integration_result(Err("Accessibility permissions cannot be queried"))
                }
            }
        },
        (InstallComponent::InputMethod, InstallAction::Install) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    use fig_integrations::input_method::{
                        InputMethod,
                    };
                    use fig_integrations::Integration;

                    integration_result(match InputMethod::default().install().await {
                        Ok(_) => Ok(()),
                        Err(err) => Err(format!("Could not install input method: {err}")),
                    })
                } else {
                    integration_result(Err("Input method install is only supported on macOS"))
                }
            }
        },
        (InstallComponent::InputMethod, InstallAction::Uninstall) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    use fig_integrations::input_method::{
                        InputMethod,
                        InputMethodError,
                    };
                    use fig_integrations::Error;
                    use fig_integrations::Integration;

                    integration_result(match InputMethod::default().uninstall().await {
                        Ok(_) | Err(Error::InputMethod(InputMethodError::CouldNotListInputSources)) => {
                            Ok(())
                        },
                        Err(err) => Err(format!("Could not uninstall input method: {err}")),
                    })
                } else {
                    integration_result(Err("Input method uninstall is only supported on macOS"))
                }
            }
        },
        (InstallComponent::InputMethod, InstallAction::Status) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    use fig_integrations::input_method::{
                        InputMethod,
                    };

                    integration_status(InputMethod::default()).await
                } else {
                    integration_unsupported()
                }
            }
        },
        (InstallComponent::DesktopEntry, action) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "linux")] {
                    if !ctx.env().in_appimage() {
                        integration_result(Err(
                            "Desktop entry installation is only supported for AppImage bundles.",
                        ))
                    } else {
                        let exec_path = ctx.env().get("APPIMAGE").map_err(super::Error::from_std)?;
                        let entry_path = ctx
                            .env()
                            .current_dir()
                            .map_err(super::Error::from_std)?
                            .join("share/applications/q-desktop.desktop");
                        let icon_path = ctx
                            .env()
                            .current_dir()
                            .map_err(super::Error::from_std)?
                            .join("share/icons/hicolor/128x128/apps/q-desktop.png");
                        let desktop_integration =
                            DesktopEntryIntegration::new(ctx, Some(entry_path), Some(icon_path), Some(exec_path.into()));
                        match action {
                            InstallAction::Install => {
                                ctx.state()
                                    .set_value("appimage.manageDesktopEntry", true)
                                    .map_err(|err| error!(?err, "unable to set `appimage.manageDesktopEntry`"))
                                    .ok();
                                integration_result(desktop_integration.install().await)
                            },
                            InstallAction::Uninstall => {
                                ctx.state()
                                    .set_value("appimage.manageDesktopEntry", false)
                                    .map_err(|err| error!(?err, "unable to set `appimage.manageDesktopEntry`"))
                                    .ok();
                                integration_result(desktop_integration.uninstall().await)
                            },
                            InstallAction::Status => integration_status(desktop_integration).await,
                        }
                    }
                } else {
                    let _ = action;
                    integration_result(Err("Desktop entry is only supported on Linux"))
                }
            }
        },
        (InstallComponent::AutostartEntry, action) => {
            cfg_if::cfg_if! {
                if #[cfg(target_os = "linux")] {
                    let ctx = ctx.context();
                    let integration = AutostartIntegration::new(&ctx).map_err(super::Error::from_std)?;
                    match action {
                        InstallAction::Install => integration_result(integration.install().await),
                        InstallAction::Uninstall => integration_result(integration.uninstall().await),
                        InstallAction::Status => integration_status(integration).await,
                    }
                } else if #[cfg(windows)] {
                    match action {
                        InstallAction::Install => {
                            integration_result(fig_integrations::launch_at_login::set_enabled(true).await)
                        },
                        InstallAction::Uninstall => {
                            integration_result(fig_integrations::launch_at_login::set_enabled(false).await)
                        },
                        InstallAction::Status => {
                            match fig_integrations::launch_at_login::is_enabled().await {
                                Ok(true) => ServerOriginatedSubMessage::InstallResponse(InstallResponse {
                                    response: Some(Response::InstallationStatus(InstallationStatus::Installed.into())),
                                }),
                                Ok(false) => ServerOriginatedSubMessage::InstallResponse(InstallResponse {
                                    response: Some(Response::InstallationStatus(
                                        InstallationStatus::NotInstalled.into(),
                                    )),
                                }),
                                Err(err) => integration_result(Err(err)),
                            }
                        },
                    }
                } else {
                    let _ = action;
                    integration_result(Err("Autostart entry is only supported on Linux and Windows"))
                }
            }
        },
        (InstallComponent::GnomeExtension, _action) => {
            integration_result(Err("The GNOME Shell extension is not supported yet"))
        },
    };

    RequestResult::Ok(Box::new(response))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[cfg(target_os = "linux")]
    use fig_integrations::desktop_entry::global_entry_path;
    use fig_os_shim::{Context, ContextProvider};
    use fig_proto::fig::server_originated_message::Submessage;
    use fig_settings::{Settings, State};
    #[cfg(target_os = "linux")]
    use fig_util::directories::{appimage_desktop_entry_icon_path, appimage_desktop_entry_path};

    use super::*;

    #[derive(Debug, Clone)]
    struct TestContext {
        ctx: Arc<Context>,
        settings: Settings,
        state: State,
    }

    impl SettingsProvider for TestContext {
        fn settings(&self) -> &Settings {
            &self.settings
        }
    }

    impl StateProvider for TestContext {
        fn state(&self) -> &fig_settings::State {
            &self.state
        }
    }

    impl ContextProvider for TestContext {
        fn context(&self) -> &Context {
            &self.ctx
        }
    }

    impl ContextArcProvider for TestContext {
        fn context_arc(&self) -> Arc<Context> {
            Arc::clone(&self.ctx)
        }
    }

    async fn assert_status(ctx: &TestContext, component: InstallComponent, expected_status: InstallationStatus) {
        let request = InstallRequest {
            component: component.into(),
            action: InstallAction::Status.into(),
        };
        let response = install(request, ctx).await.unwrap();
        assert_submessage_status(*response, expected_status, "");
    }

    fn assert_submessage_status(submessage: Submessage, expected_status: InstallationStatus, message: &str) {
        if let Submessage::InstallResponse(InstallResponse {
            response: Some(Response::InstallationStatus(actual_status)),
        }) = submessage
        {
            let expected_status: i32 = expected_status.into();
            assert_eq!(actual_status, expected_status, "{}", message);
        } else {
            panic!("unexpected response: {:?}", submessage);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_desktop_entry_installation_and_uninstallation() {
        let ctx = Context::builder()
            .with_test_home()
            .await
            .unwrap()
            .with_env_var("APPIMAGE", "/test.appimage")
            .build();
        let fs = ctx.fs();
        let entry_path = appimage_desktop_entry_path(&ctx).unwrap();
        let icon_path = appimage_desktop_entry_icon_path(&ctx).unwrap();
        fs.create_dir_all(entry_path.parent().unwrap()).await.unwrap();
        fs.write(&entry_path, "[Desktop Entry]\nExec=q-desktop").await.unwrap();
        fs.create_dir_all(icon_path.parent().unwrap()).await.unwrap();
        fs.write(&icon_path, "image").await.unwrap();
        let ctx = TestContext {
            ctx,
            settings: Settings::new_fake(),
            state: State::new_fake(),
        };

        // Test installation
        assert_status(&ctx, InstallComponent::DesktopEntry, InstallationStatus::NotInstalled).await;
        let request = InstallRequest {
            component: InstallComponent::DesktopEntry.into(),
            action: InstallAction::Install.into(),
        };
        install(request, &ctx).await.unwrap();
        assert_eq!(ctx.state.get_bool("appimage.manageDesktopEntry").unwrap(), Some(true));
        assert_status(&ctx, InstallComponent::DesktopEntry, InstallationStatus::Installed).await;

        // Test uninstallation
        let request = InstallRequest {
            component: InstallComponent::DesktopEntry.into(),
            action: InstallAction::Uninstall.into(),
        };
        install(request, &ctx).await.unwrap();
        assert_eq!(ctx.state.get_bool("appimage.manageDesktopEntry").unwrap(), Some(false));
        assert_status(&ctx, InstallComponent::DesktopEntry, InstallationStatus::NotInstalled).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_autostart_entry_installation_and_uninstallation() {
        let ctx = Context::builder().with_test_home().await.unwrap().build_fake();
        // Create global desktop entry
        {
            let global_path = global_entry_path(&ctx);
            ctx.fs().create_dir_all(global_path.parent().unwrap()).await.unwrap();
            ctx.fs().write(global_path, "[Desktop Entry]").await.unwrap();
        }
        let ctx = TestContext {
            ctx,
            settings: Settings::new_fake(),
            state: State::new_fake(),
        };

        // Test installation
        assert_status(&ctx, InstallComponent::AutostartEntry, InstallationStatus::NotInstalled).await;
        let request = InstallRequest {
            component: InstallComponent::AutostartEntry.into(),
            action: InstallAction::Install.into(),
        };
        install(request, &ctx).await.unwrap();
        assert_status(&ctx, InstallComponent::AutostartEntry, InstallationStatus::Installed).await;
        assert!(
            AutostartIntegration::to_global(&ctx).is_installed().await.is_ok(),
            "Autostart entry should have been installed."
        );

        // Test uninstallation
        let request = InstallRequest {
            component: InstallComponent::AutostartEntry.into(),
            action: InstallAction::Uninstall.into(),
        };
        install(request, &ctx).await.unwrap();
        assert_status(&ctx, InstallComponent::AutostartEntry, InstallationStatus::NotInstalled).await;
        assert!(
            AutostartIntegration::to_global(&ctx).is_installed().await.is_err(),
            "Autostart entry should have been uninstalled."
        );
    }
}
