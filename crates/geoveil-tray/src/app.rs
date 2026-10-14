//! GUI glue: the tray icon and its menu, the platform event loop, and the
//! dispatch of menu clicks. All decisions are made in [`crate::model`] and
//! [`crate::worker`]; this module only applies them.
//!
//! - Windows and macOS: a `tao` event loop on the main thread (both need a
//!   native loop on the tray's thread; macOS needs the main thread). Menu
//!   clicks and worker updates arrive as user events through an
//!   `EventLoopProxy`.
//! - Linux/BSD: the KSNI backend serves the tray over D-Bus on its own
//!   thread, so the main thread just waits on a channel.

use crate::icon;
use crate::launcher;
use crate::model::{self, ids, Action, IconKind, MenuModel, TrayState};
use crate::worker::{self, IpcLink, UiCommand, UiEvent, Worker};
use anyhow::{Context, Result};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Everything the UI thread receives.
#[derive(Debug)]
pub enum UserEvent {
    /// A menu entry was clicked.
    Menu(MenuId),
    /// The worker sent something.
    Worker(UiEvent),
}

/// The tray icon and the menu entries that change.
struct TrayUi {
    tray: TrayIcon,
    status: MenuItem,
    location: MenuItem,
    toggle: MenuItem,
    restore: MenuItem,
    quit: MenuItem,
    model: MenuModel,
}

fn make_icon(kind: IconKind) -> Result<Icon> {
    Icon::from_rgba(icon::rgba(kind), icon::SIZE, icon::SIZE).context("building the tray icon")
}

impl TrayUi {
    fn new(state: &TrayState) -> Result<Self> {
        let m = model::menu_model(state);
        let status = MenuItem::with_id(ids::STATUS, &m.status_line, false, None);
        let location = MenuItem::with_id(ids::LOCATION, &m.location_line, false, None);
        let toggle = MenuItem::with_id(ids::TOGGLE, &m.toggle.label, m.toggle.enabled, None);
        let restore = MenuItem::with_id(ids::RESTORE, "Restore original settings now", m.restore_enabled, None);
        let open_logs = MenuItem::with_id(ids::OPEN_LOGS, "Open log folder", true, None);
        let quit = MenuItem::with_id(ids::QUIT, &m.quit_label, true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &status,
            &location,
            &PredefinedMenuItem::separator(),
            &toggle,
            &restore,
            &PredefinedMenuItem::separator(),
            &open_logs,
            &quit,
        ])
        .context("building the tray menu")?;
        let tray = TrayIconBuilder::new()
            .with_id("geoveil")
            .with_menu(Box::new(menu))
            .with_tooltip(&m.tooltip)
            .with_icon(make_icon(m.icon)?)
            .build()
            .context("creating the tray icon")?;
        Ok(Self { tray, status, location, toggle, restore, quit, model: m })
    }

    fn update(&mut self, state: &TrayState) {
        let m = model::menu_model(state);
        if m == self.model {
            return;
        }
        self.status.set_text(&m.status_line);
        self.location.set_text(&m.location_line);
        self.toggle.set_text(&m.toggle.label);
        self.toggle.set_enabled(m.toggle.enabled);
        self.restore.set_enabled(m.restore_enabled);
        self.quit.set_text(&m.quit_label);
        if m.icon != self.model.icon {
            match make_icon(m.icon) {
                Ok(icon) => {
                    if let Err(e) = self.tray.set_icon(Some(icon)) {
                        tracing::warn!("cannot change the tray icon: {e}");
                    }
                }
                Err(e) => tracing::warn!("{e:#}"),
            }
        }
        if m.tooltip != self.model.tooltip {
            if let Err(e) = self.tray.set_tooltip(Some(&m.tooltip)) {
                tracing::warn!("cannot change the tooltip: {e}");
            }
        }
        self.model = m;
    }
}

/// UI-thread state shared by both event loops.
struct Controller {
    ui: Option<TrayUi>,
    state: TrayState,
    commands: UnboundedSender<UiCommand>,
    log_dir: PathBuf,
}

impl Controller {
    /// Creates the tray icon. Must run on the UI thread, once the event loop
    /// is running.
    fn show(&mut self) -> Result<()> {
        self.ui = Some(TrayUi::new(&self.state)?);
        Ok(())
    }

    /// Handles one event. Returns `true` when the application should exit.
    fn handle(&mut self, event: UserEvent) -> bool {
        match event {
            UserEvent::Worker(UiEvent::State(state)) => {
                self.state = *state;
                if let Some(ui) = self.ui.as_mut() {
                    ui.update(&self.state);
                }
                false
            }
            UserEvent::Worker(UiEvent::Exit) => {
                // Dropping the TrayIcon removes it from the tray.
                self.ui = None;
                true
            }
            UserEvent::Menu(id) => {
                let action = self.ui.as_ref().and_then(|ui| model::action_for(id.as_ref(), &ui.model));
                match action {
                    Some(action) => self.dispatch(action),
                    None => false,
                }
            }
        }
    }

    fn dispatch(&mut self, action: Action) -> bool {
        let command = match action {
            Action::OpenLogs => {
                if let Err(e) = launcher::open_folder(&self.log_dir) {
                    tracing::warn!("cannot open {}: {e}", self.log_dir.display());
                }
                return false;
            }
            Action::Pause => UiCommand::Pause,
            Action::Resume => UiCommand::Resume,
            Action::RestoreNow => UiCommand::RestoreNow,
            Action::StartDaemon => UiCommand::StartDaemon,
            Action::Quit => UiCommand::Quit,
        };
        // A send error means the worker is gone: nothing left to wait for.
        self.commands.send(command).is_err() && command == UiCommand::Quit
    }
}

/// Starts the worker thread; returns the channel for UI commands.
fn spawn_worker(link: IpcLink, auto_start: bool, sink: impl Fn(UiEvent) + Send + 'static) -> Result<UnboundedSender<UiCommand>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the IPC runtime")?;
    std::thread::Builder::new()
        .name("geoveil-ipc".into())
        .spawn(move || runtime.block_on(worker::run(Worker::new(link, auto_start), rx, sink)))
        .context("starting the IPC thread")?;
    Ok(tx)
}

/// Runs the tray until Quit.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn run(link: IpcLink, auto_start: bool, log_dir: PathBuf) -> Result<()> {
    use std::sync::Mutex;
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        // A menu-bar-only app: no Dock icon, no app menu.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = Mutex::new(event_loop.create_proxy());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Ok(p) = proxy.lock() {
            let _ = p.send_event(UserEvent::Menu(e.id));
        }
    }));
    let proxy = event_loop.create_proxy();
    let commands = spawn_worker(link, auto_start, move |e| {
        let _ = proxy.send_event(UserEvent::Worker(e));
    })?;
    let mut controller = Controller { ui: None, state: TrayState::default(), commands, log_dir };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let exit = match event {
            // The tray must be created once the loop runs (macOS requirement).
            Event::NewEvents(StartCause::Init) => match controller.show() {
                Ok(()) => false,
                Err(e) => {
                    tracing::error!("{e:#}");
                    true
                }
            },
            Event::UserEvent(ev) => controller.handle(ev),
            _ => false,
        };
        if exit {
            *control_flow = ControlFlow::Exit;
        }
    })
}

/// Runs the tray until Quit.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn run(link: IpcLink, auto_start: bool, log_dir: PathBuf) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<UserEvent>();
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = menu_tx.send(UserEvent::Menu(e.id));
    }));
    let commands = spawn_worker(link, auto_start, move |e| {
        let _ = tx.send(UserEvent::Worker(e));
    })?;
    let mut controller = Controller { ui: None, state: TrayState::default(), commands, log_dir };
    controller.show()?;
    for event in rx {
        if controller.handle(event) {
            break;
        }
    }
    Ok(())
}
