// SPDX-License-Identifier: GPL-3.0-only
//
// Small D-Bus control interface so a `--open-menu` invocation can ask the
// already-running applet to open its panel popup. The popup is a layer-shell
// surface owned by the running applet process, so a fresh process can't open it
// directly — it signals the applet over the session bus instead.

use tokio::sync::mpsc::Sender;
use zbus::{Connection, interface};

/// Well-known bus name the applet owns (matches the app ID; hyphens are valid in
/// bus names, though not in interface names or object paths).
const BUS_NAME: &str = "com.dangrover.next-meeting-app";
const OBJ_PATH: &str = "/com/dangrover/NextMeeting";
const IFACE: &str = "com.dangrover.NextMeeting.Control";

/// The served control object; each method call forwards to the applet's event
/// loop via the channel.
struct Control {
    sender: Sender<()>,
}

#[interface(name = "com.dangrover.NextMeeting.Control")]
impl Control {
    /// Ask the applet to toggle its panel popup open/closed.
    async fn open_menu(&self) {
        let _ = self.sender.send(()).await;
    }
}

/// Serve the control interface on the session bus. The returned `Connection`
/// must be kept alive to stay registered. Fails (gracefully) when no session bus
/// is available or the name is already owned.
pub async fn serve(sender: Sender<()>) -> zbus::Result<Connection> {
    zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJ_PATH, Control { sender })?
        .build()
        .await
}

/// Ask the running applet to open its menu. Returns `true` if the call reached a
/// running applet, `false` if it isn't running or the bus is unavailable.
pub async fn open_menu() -> bool {
    let Ok(conn) = Connection::session().await else {
        return false;
    };
    conn.call_method(Some(BUS_NAME), OBJ_PATH, Some(IFACE), "OpenMenu", &())
        .await
        .is_ok()
}
