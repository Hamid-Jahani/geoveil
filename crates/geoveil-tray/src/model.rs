//! Pure UI logic: tray state in, menu model out. No GUI types here, so every
//! rule is unit tested on every OS.

use geoveil_ipc::DaemonStatus;

/// Longest tooltip Windows shows (`NOTIFYICONDATAW::szTip` holds 128 UTF-16
/// units including the terminator).
pub const MAX_TOOLTIP_CHARS: usize = 127;

/// Connection to the daemon, as the tray sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    /// First attempt not finished yet.
    #[default]
    Connecting,
    /// The tray started `geoveil run` and waits for it to listen.
    Starting,
    /// Talking to the daemon.
    Connected,
    /// No daemon answers.
    Disconnected,
    /// Quit was chosen; waiting for the daemon to restore and stop.
    Stopping,
}

/// Everything the menu is built from.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrayState {
    /// Connection state.
    pub link: Link,
    /// Last status from the daemon (only meaningful while connected).
    pub status: Option<DaemonStatus>,
    /// Message for the user (failed command, daemon exited, ...).
    pub notice: Option<String>,
    /// The tray started the daemon itself (Quit then stops it).
    pub spawned: bool,
}

/// What a menu entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Pause following the VPN.
    Pause,
    /// Resume following the VPN.
    Resume,
    /// Start `geoveil run`.
    StartDaemon,
    /// Restore the original settings now.
    RestoreNow,
    /// Open the log folder.
    OpenLogs,
    /// Quit the tray (stopping a daemon it started).
    Quit,
}

/// Menu entry ids (stable strings, used as muda `MenuId`s).
pub mod ids {
    /// Status line (disabled).
    pub const STATUS: &str = "status";
    /// Country / time zone line (disabled).
    pub const LOCATION: &str = "location";
    /// Pause / Resume / Start daemon.
    pub const TOGGLE: &str = "toggle";
    /// Restore now.
    pub const RESTORE: &str = "restore";
    /// Open log folder.
    pub const OPEN_LOGS: &str = "open_logs";
    /// Quit.
    pub const QUIT: &str = "quit";
}

/// Which icon to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    /// Real settings in place (or not connected yet).
    Idle,
    /// The profile is applied.
    Engaged,
    /// Following the VPN is paused.
    Paused,
    /// Something needs attention.
    Error,
}

/// The toggle entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toggle {
    /// Label.
    pub label: String,
    /// Action when clicked.
    pub action: Action,
    /// Clickable.
    pub enabled: bool,
}

/// Text and state of every menu entry plus icon and tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    /// First line, for example "Geoveil: engaged".
    pub status_line: String,
    /// Second line, for example "DE · Europe/Berlin · de-DE".
    pub location_line: String,
    /// Pause / Resume / Start daemon.
    pub toggle: Toggle,
    /// "Restore now" clickable.
    pub restore_enabled: bool,
    /// Quit label (says when it also stops the daemon).
    pub quit_label: String,
    /// Icon.
    pub icon: IconKind,
    /// Tooltip, at most [`MAX_TOOLTIP_CHARS`] characters.
    pub tooltip: String,
}

/// Builds the menu for `s`.
pub fn menu_model(s: &TrayState) -> MenuModel {
    let connected = s.link == Link::Connected;
    let status = if connected { s.status.as_ref() } else { None };

    let status_line = match (s.link, status) {
        (Link::Connecting, _) => "Geoveil: connecting...".to_string(),
        (Link::Starting, _) => "Geoveil: starting the daemon...".to_string(),
        (Link::Stopping, _) => "Geoveil: restoring and stopping...".to_string(),
        (Link::Disconnected, _) => "Geoveil: daemon not running".to_string(),
        (Link::Connected, None) => "Geoveil: connected".to_string(),
        (Link::Connected, Some(st)) => {
            let what = if st.last_error.is_some() {
                "error, see the log"
            } else if st.paused && st.engaged {
                "paused (spoofed settings kept)"
            } else if st.paused {
                "paused"
            } else if st.engaged {
                "engaged"
            } else if st.vpn_up {
                "VPN up, applying..."
            } else {
                "watching, no VPN"
            };
            format!("Geoveil: {what}")
        }
    };

    let location_line = match status {
        Some(st) if st.engaged => match &st.profile {
            Some(p) if p.country.is_empty() => format!("Manual location · {}", p.timezone),
            Some(p) => format!("{} · {} · {}", p.country, p.timezone, p.locale),
            None => "Profile applied".to_string(),
        },
        Some(_) => "Real settings in place".to_string(),
        None => "Location: unknown".to_string(),
    };

    let toggle = match (s.link, status) {
        (Link::Connected, Some(st)) if st.paused => {
            Toggle { label: "Resume following the VPN".into(), action: Action::Resume, enabled: true }
        }
        (Link::Connected, _) => Toggle { label: "Pause".into(), action: Action::Pause, enabled: status.is_some() },
        (Link::Disconnected, _) => {
            Toggle { label: "Start the daemon".into(), action: Action::StartDaemon, enabled: true }
        }
        (Link::Connecting | Link::Starting | Link::Stopping, _) => {
            Toggle { label: "Pause".into(), action: Action::Pause, enabled: false }
        }
    };

    let restore_enabled = status.is_some_and(|st| st.engaged);

    let quit_label = if s.spawned && connected {
        "Quit (restore and stop the daemon)".to_string()
    } else {
        "Quit".to_string()
    };

    let icon = match (s.link, status) {
        (Link::Connected, Some(st)) if st.last_error.is_some() => IconKind::Error,
        (Link::Connected, Some(st)) if st.paused => IconKind::Paused,
        (Link::Connected, Some(st)) if st.engaged => IconKind::Engaged,
        (Link::Disconnected, _) if s.notice.is_some() => IconKind::Error,
        _ => IconKind::Idle,
    };

    let mut lines = vec![status_line.clone(), location_line.clone()];
    if let Some(e) = status.and_then(|st| st.last_error.as_ref()) {
        lines.push(e.clone());
    }
    if let Some(n) = &s.notice {
        lines.push(n.clone());
    }
    let tooltip = truncate_chars(&lines.join("\n"), MAX_TOOLTIP_CHARS);

    MenuModel { status_line, location_line, toggle, restore_enabled, quit_label, icon, tooltip }
}

/// The action for a clicked menu id, given the current model.
pub fn action_for(id: &str, model: &MenuModel) -> Option<Action> {
    match id {
        ids::TOGGLE if model.toggle.enabled => Some(model.toggle.action),
        ids::RESTORE if model.restore_enabled => Some(Action::RestoreNow),
        ids::OPEN_LOGS => Some(Action::OpenLogs),
        ids::QUIT => Some(Action::Quit),
        _ => None,
    }
}

/// Cuts `s` to at most `max` characters, ending with "..." when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoveil_ipc::ProfileSummary;

    fn connected(status: DaemonStatus) -> TrayState {
        TrayState { link: Link::Connected, status: Some(status), ..Default::default() }
    }

    fn de() -> ProfileSummary {
        ProfileSummary {
            country: "DE".into(),
            timezone: "Europe/Berlin".into(),
            locale: "de-DE".into(),
            lat: 52.52,
            lon: 13.405,
        }
    }

    #[test]
    fn engaged_shows_country_and_timezone() {
        let m = menu_model(&connected(DaemonStatus {
            engaged: true,
            vpn_up: true,
            profile: Some(de()),
            ..Default::default()
        }));
        assert_eq!(m.status_line, "Geoveil: engaged");
        assert_eq!(m.location_line, "DE · Europe/Berlin · de-DE");
        assert_eq!(m.toggle, Toggle { label: "Pause".into(), action: Action::Pause, enabled: true });
        assert!(m.restore_enabled);
        assert_eq!(m.icon, IconKind::Engaged);
        assert_eq!(m.quit_label, "Quit");
    }

    #[test]
    fn manual_profile_has_no_country() {
        let p = ProfileSummary { country: String::new(), timezone: "Asia/Tokyo".into(), ..de() };
        let m = menu_model(&connected(DaemonStatus { engaged: true, profile: Some(p), ..Default::default() }));
        assert_eq!(m.location_line, "Manual location · Asia/Tokyo");
    }

    #[test]
    fn watching_without_vpn() {
        let m = menu_model(&connected(DaemonStatus::default()));
        assert_eq!(m.status_line, "Geoveil: watching, no VPN");
        assert_eq!(m.location_line, "Real settings in place");
        assert!(!m.restore_enabled, "nothing to restore");
        assert_eq!(m.icon, IconKind::Idle);
    }

    #[test]
    fn vpn_up_before_engage() {
        let m = menu_model(&connected(DaemonStatus { vpn_up: true, ..Default::default() }));
        assert_eq!(m.status_line, "Geoveil: VPN up, applying...");
    }

    #[test]
    fn paused_offers_resume() {
        let m = menu_model(&connected(DaemonStatus { paused: true, ..Default::default() }));
        assert_eq!(m.status_line, "Geoveil: paused");
        assert_eq!(m.toggle.action, Action::Resume);
        assert_eq!(m.toggle.label, "Resume following the VPN");
        assert_eq!(m.icon, IconKind::Paused);

        let m = menu_model(&connected(DaemonStatus {
            paused: true,
            engaged: true,
            profile: Some(de()),
            ..Default::default()
        }));
        assert_eq!(m.status_line, "Geoveil: paused (spoofed settings kept)");
        assert!(m.restore_enabled);
    }

    #[test]
    fn daemon_error_is_shown() {
        let m = menu_model(&connected(DaemonStatus {
            engaged: true,
            last_error: Some("engaging for exit 1.2.3.4: locale failed".into()),
            ..Default::default()
        }));
        assert_eq!(m.status_line, "Geoveil: error, see the log");
        assert_eq!(m.icon, IconKind::Error);
        assert!(m.tooltip.contains("locale failed"));
    }

    #[test]
    fn disconnected_offers_start() {
        let s = TrayState { link: Link::Disconnected, ..Default::default() };
        let m = menu_model(&s);
        assert_eq!(m.status_line, "Geoveil: daemon not running");
        assert_eq!(m.location_line, "Location: unknown");
        assert_eq!(m.toggle, Toggle { label: "Start the daemon".into(), action: Action::StartDaemon, enabled: true });
        assert!(!m.restore_enabled);
        assert_eq!(m.icon, IconKind::Idle);

        let s = TrayState { notice: Some("geoveil run exited (exit code: 1)".into()), ..s };
        let m = menu_model(&s);
        assert_eq!(m.icon, IconKind::Error);
        assert!(m.tooltip.contains("exit code: 1"));
    }

    #[test]
    fn stale_status_is_ignored_when_not_connected() {
        let s = TrayState {
            link: Link::Disconnected,
            status: Some(DaemonStatus { engaged: true, profile: Some(de()), ..Default::default() }),
            ..Default::default()
        };
        let m = menu_model(&s);
        assert!(!m.restore_enabled);
        assert_eq!(m.location_line, "Location: unknown");
    }

    #[test]
    fn transitional_states_disable_actions() {
        for link in [Link::Connecting, Link::Starting, Link::Stopping] {
            let m = menu_model(&TrayState { link, ..Default::default() });
            assert!(!m.toggle.enabled, "{link:?}");
            assert!(!m.restore_enabled, "{link:?}");
            assert_eq!(action_for(ids::TOGGLE, &m), None);
            assert_eq!(action_for(ids::QUIT, &m), Some(Action::Quit));
        }
    }

    #[test]
    fn quit_label_says_when_it_stops_the_daemon() {
        let s = TrayState { spawned: true, ..connected(DaemonStatus::default()) };
        assert_eq!(menu_model(&s).quit_label, "Quit (restore and stop the daemon)");
    }

    #[test]
    fn menu_ids_map_to_actions() {
        let m = menu_model(&connected(DaemonStatus { engaged: true, ..Default::default() }));
        assert_eq!(action_for(ids::TOGGLE, &m), Some(Action::Pause));
        assert_eq!(action_for(ids::RESTORE, &m), Some(Action::RestoreNow));
        assert_eq!(action_for(ids::OPEN_LOGS, &m), Some(Action::OpenLogs));
        assert_eq!(action_for(ids::QUIT, &m), Some(Action::Quit));
        assert_eq!(action_for(ids::STATUS, &m), None);
        assert_eq!(action_for("bogus", &m), None);
        let idle = menu_model(&connected(DaemonStatus::default()));
        assert_eq!(action_for(ids::RESTORE, &idle), None, "disabled entries do nothing");
    }

    #[test]
    fn tooltip_is_capped() {
        let s = TrayState {
            link: Link::Disconnected,
            notice: Some("x".repeat(500)),
            ..Default::default()
        };
        let t = menu_model(&s).tooltip;
        assert_eq!(t.chars().count(), MAX_TOOLTIP_CHARS);
        assert!(t.ends_with("..."));
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("ééééé", 4), "é...");
    }
}
