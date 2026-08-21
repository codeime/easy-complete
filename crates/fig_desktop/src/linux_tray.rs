//! GTK tray on its own thread. tray-icon's Linux backend needs a GLib loop
//! on the thread that created the indicator; GPUI's calloop is not that loop.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use tracing::{error, warn};

use crate::tray::{get_context_menu, get_icon};

enum TrayOp {
    Reload { is_logged_in: bool },
    SetVisible(bool),
}

static TX: OnceLock<Sender<TrayOp>> = OnceLock::new();

pub fn spawn() {
    let (tx, rx) = mpsc::channel();
    if TX.set(tx).is_err() {
        return;
    }
    thread::Builder::new()
        .name("ec-gtk-tray".into())
        .spawn(move || {
            if let Err(err) = gtk::init() {
                error!(%err, "gtk init failed on tray thread");
                return;
            }
            let mut tray = match crate::tray::build_tray_icon() {
                Ok(tray) => tray,
                Err(err) => {
                    error!(%err, "Failed to create Linux tray icon");
                    return;
                },
            };
            loop {
                while let Ok(op) = rx.try_recv() {
                    match op {
                        TrayOp::Reload { is_logged_in } => {
                            tray.set_icon(Some(get_icon(is_logged_in)))
                                .map_err(|err| warn!(?err))
                                .ok();
                            tray.set_icon_as_template(true);
                            tray.set_menu(Some(Box::new(get_context_menu(is_logged_in))));
                        },
                        TrayOp::SetVisible(visible) => {
                            tray.set_visible(visible).map_err(|err| warn!(?err)).ok();
                        },
                    }
                }
                // Drain the default GLib context. This thread never runs `gtk_main`.
                let _ = gtk::glib::MainContext::default().iteration(false);
                thread::sleep(Duration::from_millis(16));
            }
        })
        .ok();
}

pub fn reload(is_logged_in: bool) {
    if let Some(tx) = TX.get() {
        tx.send(TrayOp::Reload { is_logged_in }).ok();
    }
}

pub fn set_visible(visible: bool) {
    if let Some(tx) = TX.get() {
        tx.send(TrayOp::SetVisible(visible)).ok();
    }
}
