//! macOS presenter: a `tao` event loop carrying a `tray-icon` status item with a
//! `muda` menu, driven entirely by the pure [`TrayModel`].
//!
//! Human clicks become [`TrayEvent`]s, which are appended to the activity log
//! and applied to the model; the model is then re-rendered. Nothing here
//! decides policy — it shows state and forwards clicks. The dial-out link runs
//! on a supervisor thread that only mutates the shared model and pokes the loop
//! through a user event, so every menu-bar touch stays on the main thread.
//!
//! The owner's Allow/Deny questions show in a dialog, an `NSAlert` run as a
//! sheet on a small panel of its own ([`AlertPresenter`]); [`AskDialog`]
//! decides when it shows and goes.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSButton, NSButtonType, NSControlStateValueOff, NSControlStateValueOn,
    NSModalPanelWindowLevel, NSModalResponse, NSModalResponseAbort, NSPanel, NSScrollView,
    NSTextField, NSTextView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRange, NSRect, NSSize, NSString, ns_string};

use crate::askdialog::{self, AskDialog, DialogText, Presenter};
use crate::config::TrayAppConfig;
use crate::devices::{DeviceCache, DeviceRole};
use crate::ears::{Ears, EarsMode, ModeSource};
use crate::eyes::Eyes;
use crate::hands::{self, AskBox, LogJournal};
use crate::icon::{ICON_SIZE, render_icon};
use crate::link::{self, Outbox, SshDialer, held};
use crate::log::{ActivityLog, LogError};
use crate::model::{MenuEntry, TrayEvent, TrayModel};
use crate::saylog::{SayLog, Shown, Typed, log_click, take_typed};
use crate::state::LinkState;
use crate::switch::{self, Switch};
use tulpa_stop::{Attempts, Stop};

/// How long a termination signal waits for Pause to kill and reap the running children.
const REAP_CAP: Duration = Duration::from_secs(2);
const PAUSE_ID: &str = "tulpa.pause";
const QUIT_ID: &str = "tulpa.quit";
const OPEN_CONFIG_ID: &str = "tulpa.open-config";
const EARS_LINE_ID: &str = "tulpa.ears-line";
const LOG_WINDOW_ID: &str = "tulpa.log-window";
const HANDS_FULL_ID: &str = "tulpa.hands-full";
/// The ears mode items: this prefix plus the mode's id.
const EARS_MODE_PREFIX: &str = "tulpa.ears-mode.";
/// How long "Quit" waits for the link supervisor to kill its ssh child before
/// the event loop exits anyway.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// How long Quit and the loop's end wait for the port forwards' ssh children to be gone.
const FORWARD_DRAIN: Duration = Duration::from_secs(2);

/// A wake-up the event loop asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TraySignal {
    /// The link state changed; re-render from the model.
    LinkChanged,
    /// A question appeared or went: the dialog and the menu follow.
    AskChanged,
    /// The ears' state changed (started, stopped, deaf, exited); re-render.
    EarsChanged,
    /// The eyes started or ended a photo; re-render their line.
    EyesChanged,
    /// A hands request started or ended; re-render the count.
    HandsChanged,
    /// A menu item was clicked: muda's handler forwards its id, so a click
    /// wakes the loop on its own.
    Menu(muda::MenuId),
    /// The pointer reached or clicked the menu-bar item: the menu may open now.
    TrayTouched,
    /// The device lists read in the background changed; re-render the pickers.
    DevicesChanged,
    /// A port forward opened, carried a connection or closed; re-render its rows.
    ForwardsChanged,
    /// A link serve opened, carried a peer or closed; re-render its rows.
    LinksChanged,
}

/// The clickable menu items.
#[derive(Debug)]
struct MenuIds {
    pause: muda::MenuId,
    quit: muda::MenuId,
    open_config: muda::MenuId,
    hands_full: muda::MenuId,
    ears_line: muda::MenuId,
    log_window: muda::MenuId,
}

/// The ears mode an item id selects, if it is one.
fn ears_mode_of(id: &muda::MenuId) -> Option<EarsMode> {
    id.0.strip_prefix(EARS_MODE_PREFIX)
        .and_then(EarsMode::from_id)
}

/// Opens the config in the default text editor (argv, no shell, not waited for).
fn open_config(path: &Path) -> std::io::Result<std::process::Child> {
    std::process::Command::new("/usr/bin/open")
        .arg("-t")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}

/// Opens a System Settings pane by its URL (argv, no shell, not waited for).
fn open_settings(url: &str) -> std::io::Result<std::process::Child> {
    std::process::Command::new("/usr/bin/open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}

/// The hands line with how many requests run now and the data stream's transfer.
fn hands_line(
    state: &hands::HandsState,
    paused: bool,
    in_flight: usize,
    xfer: Option<&tulpa_hands::Xfer>,
) -> String {
    hands::menu_line(state, paused, in_flight, xfer)
}

/// Posts a macOS notification with a new dialog: a heads-up only. A click on
/// it answers nothing (`osascript` posts it, so macOS opens Script Editor);
/// the owner answers in the dialog. The text goes in as an argument, never
/// spliced into the script.
fn notify_ask(line: &str) {
    let _spawned: std::io::Result<std::process::Child> =
        std::process::Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title \"tulpa\" sound name \"Ping\"",
                "-e",
                "end run",
                "--",
                line,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
}

/// The dialog of one question, on screen.
#[derive(Debug)]
struct OnScreen {
    id: u64,
    alert: Retained<NSAlert>,
    host: Retained<NSPanel>,
    allow: Retained<NSButton>,
}

/// Draws the owner's dialog: an `NSAlert` run as a sheet on a small panel of
/// its own, above other apps' windows.
///
/// A sheet, because `-[NSAlert runModal]` would hold the event loop, and with
/// it the menu's Pause and Quit, for as long as the question waits (up to
/// 900 s). A sheet is asynchronous: its completion handler gets the click and
/// the loop runs on. Tulpa has no window to attach it to (`LSUIElement`), so
/// each dialog gets a titled panel of the alert's size. objc2-app-kit 0.3.2
/// marks `beginSheetModalForWindow:completionHandler:` safe. A polled modal
/// session would need `runModalSession:`, which it marks `unsafe`, and unsafe
/// is forbidden here.
#[derive(Debug)]
struct AlertPresenter {
    mtm: MainThreadMarker,
    asks: Arc<AskBox>,
    shown: Option<OnScreen>,
}

impl AlertPresenter {
    const fn new(mtm: MainThreadMarker, asks: Arc<AskBox>) -> Self {
        Self {
            mtm,
            asks,
            shown: None,
        }
    }
}

impl Presenter for AlertPresenter {
    fn show(&mut self, ask: &hands::Asked, text: &DialogText) {
        let mtm = self.mtm;
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&text.title));
        alert.setInformativeText(&NSString::from_str(&text.body));
        // In BUTTONS order, so the response codes map back through it: Deny,
        // the default that Return presses, then Allow, which no key presses
        // and which stays off until armed. Set after the layout, so the
        // alert's own layout can't undo it.
        let [(deny_title, _), (allow_title, _)] = askdialog::BUTTONS;
        let deny = alert.addButtonWithTitle(&NSString::from_str(deny_title));
        let allow = alert.addButtonWithTitle(&NSString::from_str(allow_title));
        alert.layout();
        deny.setKeyEquivalent(ns_string!("\r"));
        allow.setKeyEquivalent(ns_string!(""));
        allow.setEnabled(false);
        let host = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), alert.window().frame().size),
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        );
        host.setTitle(ns_string!("Tulpa"));
        // Above other apps' windows, on the Space in use (a full-screen one
        // too), and kept up while another app is active.
        host.setLevel(NSModalPanelWindowLevel);
        host.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        host.setHidesOnDeactivate(false);
        host.center();
        #[expect(
            deprecated,
            reason = "its replacement, activate, exists only from macOS 14 on"
        )]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        host.makeKeyAndOrderFront(None);
        host.orderFrontRegardless();
        let asks = Arc::clone(&self.asks);
        let id = ask.id;
        let clicked = RcBlock::new(move |response: NSModalResponse| {
            // A button answers its own question by number; a sheet Tulpa
            // ended itself (NSModalResponseAbort) answers nothing.
            if let Some(answer) = response
                .checked_sub(NSAlertFirstButtonReturn)
                .and_then(askdialog::button_answer)
            {
                asks.answer(id, answer);
            }
        });
        alert.beginSheetModalForWindow_completionHandler(&host, Some(&clicked));
        notify_ask(&askdialog::notice(ask));
        self.shown = Some(OnScreen {
            id,
            alert,
            host,
            allow,
        });
    }

    fn arm(&mut self, id: u64) {
        if let Some(shown) = self.shown.as_ref().filter(|shown| shown.id == id) {
            shown.allow.setEnabled(true);
        }
    }

    fn close(&mut self, id: u64) {
        if let Some(shown) = self.shown.take_if(|shown| shown.id == id) {
            // A click ended the sheet already; otherwise Tulpa ends it.
            if shown.host.attachedSheet().is_some() {
                shown
                    .host
                    .endSheet_returnCode(&shown.alert.window(), NSModalResponseAbort);
            }
            shown.host.orderOut(None);
        }
    }
}

/// The log window (#0423): a small titled panel with the heard/said log — a
/// read-only `NSTextView` in a scroll view — and, below it, the operator's
/// line: an editable `NSTextField` beside a `Send` switch.
///
/// It has no close button (the menu's «Log window» owns its visibility) and,
/// by design, no callbacks: `AppKit`'s target/action setters are `unsafe`, and
/// unsafe is forbidden here. The `Send` control is a switch because a switch
/// holds its click by itself — the loop polls an open window every
/// [`LOG_POLL`], takes the line when the switch is on, unticks it and clears
/// the field; the log re-renders when the shared [`SayLog`] changed. The line
/// and its fate land in the log the same moment ([`send_typed_line`]). Its Debug never prints the
/// shown log, which holds typed text (#0423 r3 S3, P2).
struct LogWindow {
    /// The panel itself.
    panel: Retained<NSPanel>,
    /// The read-only log.
    text: Retained<NSTextView>,
    /// The operator's line.
    field: Retained<NSTextField>,
    /// The send switch.
    send: Retained<NSButton>,
    /// The text last set on the view, so an unchanged log costs no `setString`.
    shown: String,
    /// The window lives on the main thread; raising it activates the app there.
    mtm: MainThreadMarker,
}

/// How often an open log window is polled (#0423): the send switch and the
/// shared log. A closed window is never polled, and the loop waits for events.
const LOG_POLL: Duration = Duration::from_millis(250);
/// The window's content size, in points.
const LOG_WINDOW_W: f64 = 520.0;
const LOG_WINDOW_H: f64 = 360.0;
/// The height the bottom row (the line and the switch) keeps, in points.
const LOG_ROW_H: f64 = 44.0;

impl fmt::Debug for LogWindow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogWindow")
            .field("shown_chars", &self.shown.chars().count())
            .finish_non_exhaustive()
    }
}

impl LogWindow {
    /// Builds the window and shows it, centered.
    fn open(mtm: MainThreadMarker) -> Self {
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(LOG_WINDOW_W, LOG_WINDOW_H),
            ),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setTitle(ns_string!("Tulpa log"));
        // On every Space and above other apps' windows, like the ask dialog:
        // the fallback line matters most mid-call. Kept up while another app
        // is active, so the log stays readable beside a call.
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setHidesOnDeactivate(false);
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, LOG_ROW_H),
                NSSize::new(LOG_WINDOW_W, LOG_WINDOW_H - LOG_ROW_H),
            ),
        );
        scroll.setHasVerticalScroller(true);
        scroll.as_super().setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let text = NSTextView::initWithFrame(
            NSTextView::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(LOG_WINDOW_W, LOG_WINDOW_H - LOG_ROW_H),
            ),
        );
        // Plain text that wraps and grows down as the log grows; the scroll
        // view owns the scrolling. Not editable; selectable so a line can be
        // copied.
        text.setRichText(false);
        text.setEditable(false);
        text.setSelectable(true);
        let text_super = text.as_super();
        text_super.setHorizontallyResizable(false);
        text_super.setVerticallyResizable(true);
        text_super.setMinSize(NSSize::new(0.0, 0.0));
        text_super.setMaxSize(NSSize::new(1.0e7, 1.0e7));
        text.as_super()
            .as_super()
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        scroll.setDocumentView(Some(text.as_super().as_super()));
        // The operator's line, with a placeholder that says where it goes.
        let field = NSTextField::initWithFrame(
            NSTextField::alloc(mtm),
            NSRect::new(
                NSPoint::new(4.0, 8.0),
                NSSize::new(LOG_WINDOW_W - 108.0, 28.0),
            ),
        );
        field.setPlaceholderString(Some(ns_string!("a line to kira — tick Send")));
        field.setBezeled(true);
        field.setEditable(true);
        field.as_super().setUsesSingleLineMode(true);
        field
            .as_super()
            .as_super()
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        // The send switch: holds the click's state until the loop takes the
        // line, so no target/action (unsafe) is needed.
        let send = NSButton::initWithFrame(
            NSButton::alloc(mtm),
            NSRect::new(
                NSPoint::new(LOG_WINDOW_W - 98.0, 8.0),
                NSSize::new(92.0, 32.0),
            ),
        );
        send.setButtonType(NSButtonType::Switch);
        send.setTitle(ns_string!("Send"));
        send.setState(NSControlStateValueOff);
        if let Some(content) = panel.contentView() {
            content.addSubview(scroll.as_super());
            content.addSubview(field.as_super());
            content.addSubview(send.as_super());
        }
        panel.center();
        let window = Self {
            panel,
            text,
            field,
            send,
            shown: String::new(),
            mtm,
        };
        window.raise();
        window
    }

    /// The typed line, when the switch says send it: the switch unticks and
    /// the field clears, whatever the line was — the log carries it with its
    /// fate, so the typist sees it did (or did not) go.
    fn take_line(&self) -> Option<String> {
        if self.send.state() != NSControlStateValueOn {
            return None;
        }
        self.send.setState(NSControlStateValueOff);
        let line = self.field.as_super().stringValue().to_string();
        self.field
            .as_super()
            .setStringValue(&NSString::from_str(""));
        Some(line)
    }

    /// Puts the log on the view if it changed, and keeps the end in sight.
    fn show(&mut self, rendered: &str) {
        if rendered == self.shown {
            return;
        }
        rendered.clone_into(&mut self.shown);
        let text = self.text.as_super();
        text.setString(&NSString::from_str(rendered));
        // NSRange counts UTF-16 units, as NSString does: a char count stops short of the end.
        let end = NSRange {
            location: rendered.encode_utf16().count(),
            length: 0,
        };
        text.scrollRangeToVisible(end);
    }
}

impl Shown for LogWindow {
    /// Whether the window is on screen (a hidden panel is not).
    fn is_open(&self) -> bool {
        self.panel.isVisible()
    }

    /// Puts the window on screen, in front, the app active.
    fn raise(&self) {
        #[expect(
            deprecated,
            reason = "its replacement, activate, exists only from macOS 14 on"
        )]
        NSApplication::sharedApplication(self.mtm).activateIgnoringOtherApps(true);
        self.panel.makeKeyAndOrderFront(None);
        self.panel.orderFrontRegardless();
    }

    /// Hides the window; the menu item that opened it unticks.
    fn close(&self) {
        self.panel.orderOut(None);
    }
}

/// Handles one line the operator sent from the window (#0423) on the loop's thread — the `AppKit`
/// layer never decides anything: [`take_typed`] marks it and queues it, and the journal notes the
/// fate, never the text: the operator's words are nobody's log line.
fn send_typed_line(
    line: &str,
    saylog: &Mutex<SayLog>,
    model: &Mutex<TrayModel>,
    outbox: &Outbox,
    log: &mut ActivityLog,
) {
    let state = model
        .lock()
        .map_or(LinkState::Off, |tray| tray.link_state());
    let fate = take_typed(saylog, outbox, line, SystemTime::now());
    let _noted: Result<(), LogError> = log.note(&format!(
        "window: one typed line {}",
        match (&fate, state == LinkState::Connected) {
            (Typed::Refused(_), _) => "refused",
            (_, true) => "handed to the link",
            (_, false) => "queued for the next connection",
        }
    ));
}

/// Serves an open log window on a wake (#0423): the typed line goes when its
/// switch is on, then the log shows if it changed. A closed window returns at
/// once — the loop's wait then ignores the poll.
fn service_log_window(
    log_window: &mut Option<LogWindow>,
    saylog: &Arc<Mutex<SayLog>>,
    model: &Mutex<TrayModel>,
    outbox: &Outbox,
    log: &mut ActivityLog,
) {
    let Some(window) = log_window.as_mut().filter(|window| window.is_open()) else {
        return;
    };
    if let Some(line) = window.take_line() {
        send_typed_line(&line, saylog, model, outbox, log);
    }
    let rendered = held(saylog).render();
    window.show(&rendered);
}

/// The loop's wait: until the dialog's next moment (Allow armed, or its
/// deadline) or, while the log window is open, the next poll — whichever
/// comes first — or for the next event.
fn wait_for<P: Presenter>(
    dialog: &AskDialog<P>,
    log_window_open: bool,
) -> tao::event_loop::ControlFlow {
    let poll = if log_window_open {
        Instant::now().checked_add(LOG_POLL)
    } else {
        None
    };
    match (dialog.next_wake(), poll) {
        (Some(dialog_at), Some(poll_at)) => {
            tao::event_loop::ControlFlow::WaitUntil(dialog_at.min(poll_at))
        }
        (Some(at), None) | (None, Some(at)) => tao::event_loop::ControlFlow::WaitUntil(at),
        (None, None) => tao::event_loop::ControlFlow::Wait,
    }
}

/// Failure to start or run the menu-bar presenter.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunError {
    /// The activity log could not be opened.
    Log(LogError),
    /// The rendered icon was rejected by tray-icon.
    Icon(String),
    /// The status item could not be created.
    Tray(String),
    /// The event loop failed.
    EventLoop(String),
    /// Another tulpa holds the instance lock and did not let go.
    Instance(crate::lifecycle::InstanceError),
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(error) => write!(formatter, "activity log: {error}"),
            Self::Icon(error) => write!(formatter, "menu bar icon: {error}"),
            Self::Tray(error) => write!(formatter, "status item: {error}"),
            Self::EventLoop(error) => write!(formatter, "event loop: {error}"),
            Self::Instance(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Log(error) => Some(error),
            Self::Instance(error) => Some(error),
            Self::Icon(_) | Self::Tray(_) | Self::EventLoop(_) => None,
        }
    }
}

/// Runs the menu-bar presenter until "Quit" is clicked. Does not return: the
/// event loop runs until the process exits.
///
/// # Errors
///
/// Returns an error when the activity log cannot be opened. Icon and status
/// item failures during the run are noted to the log and stop the loop.
#[expect(
    clippy::too_many_lines,
    reason = "one tao event-loop closure; macOS-only, Linux clippy never sees it"
)]
pub fn run(config: &TrayAppConfig, config_path: &Path) -> Result<(), RunError> {
    let config_path: PathBuf = config_path.to_path_buf();
    // Before any thread starts: the termination signals go to one watcher
    // thread (below), so a kill or a logout runs the shutdown instead of
    // leaving the capture group behind (seen live).
    let termination = crate::lifecycle::TerminationWatch::block();
    let mut log = ActivityLog::open(&config.log_path()).map_err(RunError::Log)?;
    // flow.log beside tulpa.log: every frame over the link, before the link can dial.
    let flow_path = config.log_path().with_file_name(crate::flow::FILE_NAME);
    if crate::flow::init(&flow_path, &config.log_path()) {
        let _noted: Result<(), LogError> = log.note(&format!(
            "flow: every link frame to {}",
            flow_path.display()
        ));
    }
    // One tulpa at a time: a second one (a relaunch outside launchd) takes over
    // from the first — SIGTERM to it, its watcher shuts it down, the lock frees.
    let lock_path = config.log_path().with_file_name("tulpa.lock");
    let _instance = crate::lifecycle::SingleInstance::acquire(
        &lock_path,
        crate::lifecycle::TAKEOVER_WAIT,
        |pid| {
            let _noted: Result<(), LogError> = log.note(&format!(
                "another tulpa (pid {pid}) is running: taking over"
            ));
            crate::lifecycle::terminate(pid);
        },
    )
    .map_err(RunError::Instance)?;
    let model = Arc::new(Mutex::new(TrayModel::new(
        config.initial_link_state(),
        config.host_alias(),
        config.organ_modes(),
    )));
    let stop = Arc::new(Stop::new());
    let (supervisor_done_tx, supervisor_done_rx) = mpsc::channel::<()>();
    let mut event_loop = tao::event_loop::EventLoopBuilder::<TraySignal>::with_user_event().build();
    // Only the menu-bar icon, not the Dock: tao applies its own activation policy at
    // launch — Regular by default — which overrides Info.plist's LSUIElement and puts Tulpa in the Dock.
    tao::platform::macos::EventLoopExtMacOS::set_activation_policy(
        &mut event_loop,
        tao::platform::macos::ActivationPolicy::Accessory,
    );
    let proxy = event_loop.create_proxy();
    // Every menu click wakes the loop: muda hands it to this handler instead of its channel, which the
    // loop read only when something else woke it, so a lone click was lost.
    let menu_proxy = proxy.clone();
    muda::MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
        let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
            menu_proxy.send_event(TraySignal::Menu(event.id));
    }));
    // The pointer on the menu-bar item: a device list whose last read failed is read again (in the
    // background); device changes themselves come as CoreAudio events (below).
    let touch_proxy = proxy.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event: tray_icon::TrayIconEvent| {
        if matches!(
            event,
            tray_icon::TrayIconEvent::Click { .. } | tray_icon::TrayIconEvent::Enter { .. }
        ) {
            let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                touch_proxy.send_event(TraySignal::TrayTouched);
        }
    }));

    // Hands: the flag Pause/Quit own (true = every verb refused, a running
    // child killed), the one-at-a-time question box, the log as journal.
    let paused = Arc::new(Stop::new());
    let ask_proxy = proxy.clone();
    let ask_box = Arc::new(AskBox::new(Box::new(move || {
        let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
            ask_proxy.send_event(TraySignal::AskChanged);
    })));
    let journal = Arc::new(LogJournal::open(&config.log_path()).map_err(RunError::Log)?);
    // The link's attempts, shared between the dialer, the hands and the eyes:
    // each pipe is an attempt whose stop its reader sets when the ssh child's
    // stdout ends, never cleared; a running hands child is killed on the stop
    // of the attempt its request came on.
    let link_attempts = Arc::new(Attempts::default());
    // Ears: the capture runs as this process's own child (TCC attributes the
    // microphone to the app); it ignores the link's liveness, dies with Pause
    // and Quit, and speaks up through the link's outbox.
    let outbox = Arc::new(Outbox::default());
    // The log window's shared log (#0423): the ears and the window's send
    // path write it, the window renders it, both through the one lock.
    let saylog = Arc::new(Mutex::new(SayLog::new()));
    // #0423 (akira F2/T2, r2): the wire's word for a queued line, typed or heard,
    // flips its window mark. Registered here, once, where both exist.
    crate::saylog::attach(&saylog, &outbox);
    let ears_proxy = proxy.clone();
    let ears_journal = Arc::clone(&journal);
    let ears_journal: Arc<dyn tulpa_hands::Journal> = ears_journal;
    let ears = Arc::new(
        Ears::build(
            config,
            Arc::clone(&paused),
            Arc::clone(&outbox),
            ears_journal,
            Box::new(move || {
                let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                    ears_proxy.send_event(TraySignal::EarsChanged);
            }),
        )
        .with_mode_file(crate::ears::mode_file_beside(&config_path))
        .with_lid(Arc::new(crate::ears::IoregLid::new(
            crate::config::expand_tilde("~"),
            Arc::clone(&paused),
        )))
        .with_saylog(Arc::clone(&saylog)),
    );
    // The build check's outcome marker, written by the login LaunchAgent's
    // check (packaging/macos/build-check.sh): watched here and spoken to the
    // link as a `build-check` event each time its meaning changes, so a Mac
    // that fell behind — or failed to catch up — becomes visible on the bus.
    let buildcheck_journal = Arc::clone(&journal);
    let buildcheck_journal: Arc<dyn tulpa_hands::Journal> = buildcheck_journal;
    let buildcheck = Arc::new(crate::buildcheck::BuildCheckWatch::new(
        crate::config::expand_tilde(crate::buildcheck::BUILD_CHECK_PATH),
        Arc::clone(&outbox),
        buildcheck_journal,
    ));
    buildcheck.spawn(crate::buildcheck::BUILD_CHECK_EVERY);

    // The Mac's audio devices for the «Ears mic» and «Voice output» pickers, read in the background.
    let devices_proxy = proxy.clone();
    let devices_journal = Arc::clone(&journal);
    let devices_journal: Arc<dyn tulpa_hands::Journal> = devices_journal;
    // The keep-awake logs a caffeinate that keeps ending, once per episode of restarts.
    let awake_journal = Arc::clone(&journal);
    let awake_journal: Arc<dyn tulpa_hands::Journal> = awake_journal;
    let devices = Arc::new(
        DeviceCache::new(
            PathBuf::from(crate::devices::SYSTEM_PROFILER),
            crate::config::expand_tilde("~"),
            Arc::clone(&paused),
            devices_journal,
            Box::new(move || {
                let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                    devices_proxy.send_event(TraySignal::DevicesChanged);
            }),
        )
        .listening_with(Box::new(|on_event| {
            tulpa_audio_events::listen(move |_| on_event())
                .map(|held| Box::new(held) as Box<dyn Send>)
                .map_err(|e| e.to_string())
        })),
    );
    match termination {
        Ok(watch) => {
            let term_ears = Arc::clone(&ears);
            let term_paused = Arc::clone(&paused);
            let term_ask = Arc::clone(&ask_box);
            let term_journal = Arc::clone(&journal);
            let _watcher = watch.spawn(move |signal| {
                tulpa_hands::Journal::record(&*term_journal, &format!("{signal}: shutting down"));
                term_paused.set();
                term_ask.deny_all();
                term_ears.shutdown();
                // Every child the tray runs is given this Pause (the hands' jobs, the device
                // and lid reads), so it wakes each one's wait, which kills its group and reaps
                // it: wait for that (capped) before the process goes. The watch then ends it by
                // the signal, not a successful exit: launchd starts Tulpa again.
                if !tulpa_hands::exec::wait_reaped(REAP_CAP) {
                    tulpa_hands::Journal::record(
                        &*term_journal,
                        &format!("{signal}: a child was not reaped within {REAP_CAP:?}"),
                    );
                }
            });
        }
        Err(error) => {
            let _noted: Result<(), LogError> = log.note(&format!(
                "termination signals not taken ({error}): a kill leaves children"
            ));
        }
    }
    // Eyes: one camera still on request, taken by the bundle's helper as this process's own
    // child (macOS attributes the camera to the app), refused while paused.
    let eyes_proxy = proxy.clone();
    let eyes_journal = Arc::clone(&journal);
    let eyes_journal: Arc<dyn tulpa_hands::Journal> = eyes_journal;
    let eyes_asker = Arc::clone(&ask_box);
    let eyes_asker: Arc<dyn tulpa_hands::Asker> = eyes_asker;
    let eyes = Arc::new(Eyes::build(
        config,
        Arc::clone(&paused),
        Arc::clone(&link_attempts),
        eyes_asker,
        eyes_journal,
        Box::new(move || {
            let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                eyes_proxy.send_event(TraySignal::EyesChanged);
        }),
    ));
    // Photos not fetched within 24 h go at start too, not only with the next eyes.photo.
    eyes.sweep();
    // The data stream: each transfer dials a second ssh child on the route the link is on;
    // its byte count shows on the hands line. The eyes' photos are its shelf while they are on.
    let link_route: link::CurrentRoute = Arc::new(Mutex::new(None));
    // Port forwards on request, on the link's route: only with a link, only on [forward] ports. An ssh an earlier
    // run left (a SIGKILL to its whole group, a power cut) is killed first.
    let forwards: Option<Arc<crate::forward::Forwards>> = match config.dial_spec() {
        Ok(Some(spec)) => {
            let forward_journal: Arc<dyn tulpa_hands::Journal> =
                Arc::clone(&journal) as Arc<dyn tulpa_hands::Journal>;
            let pid_file = config.forward_pid_path();
            let _swept: usize =
                crate::forward::sweep_leftovers(&pid_file, forward_journal.as_ref());
            let spawn: Arc<dyn crate::forward::Spawn> = Arc::new(crate::forward::SshSpawn::new(
                spec,
                Arc::clone(&link_route),
                pid_file,
                Arc::clone(&forward_journal),
            ));
            let forward_proxy = proxy.clone();
            Some(Arc::new(crate::forward::Forwards::new(
                config.forward_ports(),
                spawn,
                Arc::clone(&paused),
                forward_journal,
                Arc::new(move || {
                    let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                        forward_proxy.send_event(TraySignal::ForwardsChanged);
                }),
            )))
        }
        Ok(None) | Err(_) => None,
    };
    // Link serves on request (#0375), on the link's route: only with a link, only on [link] ports.
    // The streams are ssh -W children like the data stream's; the hub tears the link down when this
    // Mac's own link drops, which ends them — no pid file of their own.
    let links: Option<Arc<crate::linkserve::LinkServe>> = match config.dial_spec() {
        Ok(Some(spec)) => {
            let link_journal: Arc<dyn tulpa_hands::Journal> =
                Arc::clone(&journal) as Arc<dyn tulpa_hands::Journal>;
            let link_dial: Arc<dyn crate::linkserve::LinkDial> = Arc::new(
                crate::link::SshLinkDial::new(spec, config.log_path(), Arc::clone(&link_route)),
            );
            let link_proxy = proxy.clone();
            Some(Arc::new(crate::linkserve::LinkServe::new(
                config.link_ports(),
                link_dial,
                Arc::clone(&paused),
                link_journal,
                Arc::new(move || {
                    let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                        link_proxy.send_event(TraySignal::LinksChanged);
                }),
            )))
        }
        Ok(None) | Err(_) => None,
    };
    let xfer_proxy = proxy.clone();
    let xfer = hands::data_stream(
        config,
        &link_route,
        Box::new(move || {
            let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                xfer_proxy.send_event(TraySignal::HandsChanged);
        }),
        eyes.shelf(),
    );
    // Full hands (any shell command, any file), from the policy and toggled from the menu.
    let hands_full = Arc::new(AtomicBool::new(false));
    let (hands_state, engine) = hands::build(
        config,
        Arc::clone(&ask_box),
        Arc::clone(&journal),
        Arc::clone(&paused),
        Arc::clone(&link_attempts),
        Arc::clone(&hands_full),
        xfer.clone(),
    );
    let has_hands = engine.is_some();
    let policy_file: Option<PathBuf> = config
        .hands
        .policy
        .as_deref()
        .map(crate::config::expand_tilde);
    // The ears come back in the mode last chosen: an app update or a relaunch restarts the app.
    ears.restore_mode();
    if let Ok(mut tray) = model.lock() {
        tray.set_hands_line(Some(hands_line(&hands_state, false, 0, xfer.as_deref())));
        tray.set_ears_line(Some(ears.line(false)));
        tray.set_ears_mode(ears.is_on().then(|| ears.mode()));
        tray.set_eyes_line(eyes.line(false));
        tray.set_hands_full(has_hands.then(|| hands_full.load(Ordering::SeqCst)));
    }
    // Every device change reads the lists again (the ears' mic follows the same events). A listener that
    // CoreAudio refuses is said in the log and the menu, and registered again when the menu opens: never a clock.
    devices.listen();
    show_devices(&model, &ears, &devices);
    devices.invalidate();
    // The Mac stays awake while Tulpa runs ([keepawake] always, below), and while the agent works on it:
    // hands in flight, ears on.
    let awake_proxy = proxy.clone();
    let (awake_flags, refused) =
        crate::keepawake::configured_flags(config.keepawake.flags.as_deref());
    if let Some(line) = refused {
        let _noted: Result<(), LogError> = log.note(&line);
    }
    let keep_awake = awake_flags.keep_awake(Box::new(move || {
        let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
            awake_proxy.send_event(TraySignal::HandsChanged);
    }));
    let hands_awake = Arc::clone(&keep_awake);
    // The hands' verbs at work: only these keep the Mac awake — never the broker's idle ping
    // (every 60 s, under the 120 s grace) or a refusal (with them counted, the hold never let go).
    let working = Arc::new(AtomicUsize::new(0));
    let work_hook: tulpa_hands::WorkHook = Arc::new(move |on: bool| {
        if on {
            if working.fetch_add(1, Ordering::SeqCst) == 0 {
                hands_awake.hold("hands");
            }
        } else if working.fetch_sub(1, Ordering::SeqCst) == 1 {
            hands_awake.release("hands");
        }
    });
    keep_awake.log_to(awake_journal);
    // Tulpa's own keep-awake is the Mac's only one: held from here until Tulpa exits unless
    // `[keepawake] always = false`, and shown in the menu, the log and `ears.status`.
    if config.keepawake.always {
        keep_awake.hold(crate::keepawake::ALWAYS);
    }
    let _noted: Result<(), LogError> = log.note(&keep_awake.line());
    let status_awake = Arc::clone(&keep_awake);
    ears.report_keepawake(Box::new(move || status_awake.status()));
    if let Ok(mut tray) = model.lock() {
        tray.set_awake_line(Some(keep_awake.line()));
    }
    // The hands' requests in flight, counted around the engine for the menu.
    let in_flight = Arc::new(AtomicUsize::new(0));
    let hands_proxy = proxy.clone();
    let counter = Arc::clone(&in_flight);
    let hands_handler: Option<link::LineHandler> = engine.map(|engine| {
        let engine = Arc::new(engine.with_working(work_hook));
        let handler: link::LineHandler = Arc::new(move |line: &[u8]| {
            counter.fetch_add(1, Ordering::SeqCst);
            let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                hands_proxy.send_event(TraySignal::HandsChanged);
            let reply = engine.handle(line);
            counter.fetch_sub(1, Ordering::SeqCst);
            let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                hands_proxy.send_event(TraySignal::HandsChanged);
            reply
        });
        handler
    });
    // The photo just taken is fetched past the hands, so in any hands mode.
    let handler = crate::eyes::route(
        crate::ears::route(hands_handler, Arc::clone(&ears)),
        Arc::clone(&eyes),
        xfer.clone(),
    );
    // The files channel rides the link beside the senses (#0395): a bundle
    // lands whatever the hands' mode and whatever is paused, on its own data
    // stream so it never queues behind a hands transfer.
    let files_working = Arc::new(AtomicUsize::new(0));
    let files_awake = Arc::clone(&keep_awake);
    let files_work_hook: tulpa_hands::WorkHook = Arc::new(move |on: bool| {
        if on {
            if files_working.fetch_add(1, Ordering::SeqCst) == 0 {
                files_awake.hold("files");
            }
        } else if files_working.fetch_sub(1, Ordering::SeqCst) == 1 {
            files_awake.release("files");
        }
    });
    let files = crate::files::build(
        config,
        &link_route,
        Arc::clone(&journal) as Arc<dyn tulpa_hands::Journal>,
        Some(files_work_hook),
        Arc::clone(&outbox),
    );
    let handler = crate::files::route(handler, files);
    // forward.* first: a forward is no organ's; the rest as before.
    let handler = crate::forward::route(handler, forwards.clone());
    // link.serve-* beside them: the hub's link.open asks, the streams dial from here.
    let handler = crate::linkserve::route(handler, links.clone());

    // The route the link is on, for the menu.
    let route_line = Arc::new(Mutex::new(String::new()));
    let menu_route = Arc::clone(&route_line);
    match config.dial_spec() {
        Ok(Some(spec)) => {
            let dialer = SshDialer::new(spec, config.log_path())
                .with_route_line(Arc::clone(&route_line))
                .with_current_route(Arc::clone(&link_route))
                .with_handler(handler)
                .with_liveness(Arc::clone(&link_attempts))
                .with_outbox(Arc::clone(&outbox));
            let thread_model = Arc::clone(&model);
            let link_ears = Arc::clone(&ears);
            let thread_stop = Arc::clone(&stop);
            let thread_proxy = proxy;
            let log_path = config.log_path();
            // Taken now, before a later install can put another executable under the same path.
            let checkin = crate::checkin::CheckIn::for_this_process();
            std::thread::spawn(move || {
                let mut report = |state: LinkState| {
                    // Each accepted hello: the hub hears the ears' mode, whatever it missed while down.
                    if state == LinkState::Connected {
                        link_ears.link_up();
                    }
                    let Ok(mut tray) = thread_model.lock() else {
                        return;
                    };
                    tray.set_link_state(state);
                    let _sent: Result<(), tao::event_loop::EventLoopClosed<TraySignal>> =
                        thread_proxy.send_event(TraySignal::LinkChanged);
                };
                let mut rest = |wait: Duration| link::rest_until(wait, &thread_stop);
                link::supervise(
                    &dialer,
                    &log_path,
                    Some(&checkin),
                    &thread_stop,
                    &mut report,
                    &mut rest,
                );
                let _sent: Result<(), mpsc::SendError<()>> = supervisor_done_tx.send(());
            });
        }
        Ok(None) => {}
        Err(error) => {
            let _noted: Result<(), LogError> = log.note(&format!("link: not dialing: {error}"));
            if let Ok(mut tray) = model.lock() {
                tray.set_link_state(LinkState::Off);
            }
        }
    }

    let ids = MenuIds {
        pause: muda::MenuId::from(PAUSE_ID),
        quit: muda::MenuId::from(QUIT_ID),
        open_config: muda::MenuId::from(OPEN_CONFIG_ID),
        hands_full: muda::MenuId::from(HANDS_FULL_ID),
        ears_line: muda::MenuId::from(EARS_LINE_ID),
        log_window: muda::MenuId::from(LOG_WINDOW_ID),
    };
    let mut tray: Option<tray_icon::TrayIcon> = None;
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(RunError::EventLoop(
            "the menu bar runs on the main thread only".to_owned(),
        ));
    };
    let mut dialog = AskDialog::new(AlertPresenter::new(mtm, Arc::clone(&ask_box)));
    let mut log_window: Option<LogWindow> = None;

    event_loop.run(move |event, _window_target, control_flow| {
        use tao::event::{Event, StartCause};
        use tao::event_loop::ControlFlow;
        // Woken for the dialog's next moment, if one is up; Quit below exits.
        // An open log window is served on every wake, poll or signal alike
        // (#0423); a closed one costs nothing.
        service_log_window(&mut log_window, &saylog, &model, &outbox, &mut log);
        *control_flow = wait_for(&dialog, log_window.as_ref().is_some_and(LogWindow::is_open));

        if event == Event::NewEvents(StartCause::Init) {
            match build_tray(&model, &ids) {
                Ok(item) => tray = Some(item),
                Err(message) => {
                    let _noted: Result<(), LogError> = log.note(&message);
                    // a tray that failed to appear must not look like success:
                    // run() diverges, so a nonzero exit code is the only honest signal
                    #[expect(clippy::exit, reason = "tao run() diverges; a failed tray must exit nonzero, not look like success")]
                    std::process::exit(1);
                }
            }
        }

        if matches!(event, Event::UserEvent(TraySignal::AskChanged)) {
            let first = ask_box.head();
            dialog.sync(first.as_ref(), Instant::now());
            *control_flow = wait_for(&dialog, log_window.as_ref().is_some_and(LogWindow::is_open));
            if let Ok(mut locked) = model.lock() {
                locked.set_ask(first.map(|asked| (asked.id, asked.line())));
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        // The dialog's own clock: Allow armed, or the dialog closed at its question's deadline, even before the
        // engine's clock takes the question out of the line.
        if matches!(event, Event::NewEvents(StartCause::ResumeTimeReached { .. })) {
            dialog.sync(ask_box.head().as_ref(), Instant::now());
            *control_flow = wait_for(&dialog, log_window.as_ref().is_some_and(LogWindow::is_open));
        }

        if matches!(event, Event::UserEvent(TraySignal::LinkChanged)) {
            if let (Ok(mut locked), Ok(route)) = (model.lock(), menu_route.lock()) {
                locked.set_route_line(Some(route.clone()));
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        if matches!(event, Event::UserEvent(TraySignal::EarsChanged)) {
            if ears.mode() == EarsMode::Off {
                keep_awake.release("ears");
            } else {
                keep_awake.hold("ears");
            }
            if let Ok(mut locked) = model.lock() {
                locked.set_ears_line(Some(ears.line(paused.is_set())));
                locked.set_ears_mode(ears.is_on().then(|| ears.mode()));
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        if matches!(event, Event::UserEvent(TraySignal::EyesChanged)) {
            if let Ok(mut locked) = model.lock() {
                locked.set_eyes_line(eyes.line(paused.is_set()));
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        if matches!(event, Event::UserEvent(TraySignal::HandsChanged)) {
            if let Ok(mut locked) = model.lock() {
                // A transfer that ends may have handed a photo over: the waiting count changes.
                locked.set_eyes_line(eyes.line(paused.is_set()));
                locked.set_awake_line(Some(keep_awake.line()));
                locked.set_hands_line(Some(hands_line(
                    &hands_state,
                    paused.is_set(),
                    in_flight.load(Ordering::SeqCst),
                    xfer.as_deref(),
                )));
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        if matches!(event, Event::UserEvent(TraySignal::TrayTouched)) {
            let unheard = devices.unheard();
            devices.retry_failed();
            // The pickers' error line goes once the events are heard again.
            if devices.unheard() != unheard {
                show_devices(&model, &ears, &devices);
                refresh(&mut tray, &model, &ids, &mut log);
            }
        }

        if matches!(event, Event::UserEvent(TraySignal::DevicesChanged)) {
            show_devices(&model, &ears, &devices);
            refresh(&mut tray, &model, &ids, &mut log);
        }

        if matches!(event, Event::UserEvent(TraySignal::ForwardsChanged)) {
            if let (Some(forwards), Ok(mut locked)) = (&forwards, model.lock()) {
                locked.set_forwards(forwards.list());
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        if matches!(event, Event::UserEvent(TraySignal::LinksChanged)) {
            if let (Some(links), Ok(mut locked)) = (&links, model.lock()) {
                locked.set_links(links.list());
            }
            refresh(&mut tray, &model, &ids, &mut log);
        }

        // However the loop ends, the capture child does not outlive it.
        if event == Event::LoopDestroyed {
            keep_awake.shutdown();
            ears.shutdown();
            if let Some(forwards) = &forwards {
                forwards.close_all("Tulpa ended");
                forwards.drain(FORWARD_DRAIN);
            }
            if let Some(links) = &links {
                links.close_all("Tulpa ended");
                links.drain(FORWARD_DRAIN);
            }
        }

        if let Event::UserEvent(TraySignal::Menu(menu_id)) = &event {
            if *menu_id == ids.hands_full {
                let now_full = !hands_full.load(Ordering::SeqCst);
                hands_full.store(now_full, Ordering::SeqCst);
                crate::switch::announce(
                    &outbox,
                    Switch::HandsFull,
                    switch::on_off(!now_full),
                    switch::on_off(now_full),
                    SystemTime::now(),
                );
                // Kept across restarts — written into the policy file.
                if let Some(policy) = &policy_file
                    && let Err(error) = hands::persist_full(policy, now_full)
                {
                    let _noted: Result<(), LogError> = log.note(&format!("hands: full mode not saved to the policy ({error}); a restart reverts it"));
                }
                let _noted: Result<(), LogError> = log.note(&format!(
                    "hands: full mode {} from the menu",
                    if now_full { "ON (any shell command, any file)" } else { "off" }
                ));
                if let Ok(mut locked) = model.lock() {
                    locked.set_hands_full(Some(now_full));
                }
                refresh(&mut tray, &model, &ids, &mut log);
            } else if *menu_id == ids.open_config {
                if let Err(error) = open_config(&config_path) {
                    let _noted: Result<(), LogError> =
                        log.note(&format!("open config: {error}"));
                }
            } else if *menu_id == ids.log_window {
                // #0423: the item owns the window's visibility — it has no
                // close button, so the two cannot disagree; every click flips it.
                let open = log_click(&mut log_window, || LogWindow::open(mtm));
                if let Ok(mut locked) = model.lock() {
                    locked.set_log_window(open);
                }
                service_log_window(&mut log_window, &saylog, &model, &outbox, &mut log);
                refresh(&mut tray, &model, &ids, &mut log);
                *control_flow = wait_for(&dialog, open);
            } else if *menu_id == ids.ears_line {
                // A DEGRADED side's line opens its Settings pane; any other ears line opens nothing.
                if let Some(Err(error)) = ears.fix_url().map(open_settings) {
                    let _noted: Result<(), LogError> = log.note(&format!("open settings: {error}"));
                }
            } else if let Some(mode) = ears_mode_of(menu_id) {
                // Logged by the organ (switch or refusal); the menu re-renders on EarsChanged.
                let was = ears.mode();
                let _switched: Result<String, String> = ears.set_mode(mode, ModeSource::Menu);
                crate::switch::announce(
                    &outbox,
                    Switch::EarsMode,
                    was.id(),
                    ears.mode().id(),
                    SystemTime::now(),
                );
                if let Ok(mut locked) = model.lock() {
                    locked.set_ears_mode(ears.is_on().then(|| ears.mode()));
                    locked.set_ears_line(Some(ears.line(paused.is_set())));
                }
                refresh(&mut tray, &model, &ids, &mut log);
            } else if let Some((role, name)) = crate::devices::pick_of(&menu_id.0) {
                // Applied, then kept in the config; the line says both (or why not).
                let picked = || match role {
                    DeviceRole::Mic => ears.mic(),
                    DeviceRole::Output => ears.output(),
                };
                let was = picked();
                let (Ok(line) | Err(line)) =
                    crate::devices::pick(&ears, &config_path, role, name.as_deref());
                crate::switch::announce(
                    &outbox,
                    Switch::Device(role),
                    switch::device(was.as_deref()),
                    switch::device(picked().as_deref()),
                    SystemTime::now(),
                );
                let _noted: Result<(), LogError> = log.note(&line);
                show_devices(&model, &ears, &devices);
                if let Ok(mut locked) = model.lock() {
                    locked.set_ears_line(Some(ears.line(paused.is_set())));
                }
                refresh(&mut tray, &model, &ids, &mut log);
            } else if let Some((port, dir)) = crate::forward::close_item_of(&menu_id.0) {
                // Logged by the forwards; the row goes on ForwardsChanged.
                if let Some(forwards) = &forwards {
                    forwards.close(port, dir, "closed from the menu");
                }
            } else if let Some(link) = crate::linkserve::close_item_of(&menu_id.0) {
                // Logged by the serve; the row goes on LinksChanged.
                if let Some(links) = &links {
                    links.close(&link, "closed from the menu");
                }
            } else if let Some((ask, answer)) = crate::askdialog::menu_answer(&menu_id.0) {
                // The item carries its question's number: one clicked in a menu shown before that
                // question went answers nothing.
                ask_box.answer(ask, answer);
            } else if *menu_id == ids.pause {
                match model.lock() {
                    Ok(mut locked) => {
                        let event = locked.toggle_pause();
                        // The organ gate: Pause controls the hands for real —
                        // verbs refused, a running child's group killed, a
                        // waiting question denied.
                        let now_paused = locked.is_paused();
                        paused.set_to(now_paused);
                        locked.set_hands_line(Some(hands_line(
                            &hands_state,
                            now_paused,
                            in_flight.load(Ordering::SeqCst),
                            xfer.as_deref(),
                        )));
                        drop(locked);
                        if now_paused {
                            ask_box.deny_all();
                            // Every port forward and link serve closes; resume opens none (the agent asks again).
                            if let Some(forwards) = &forwards {
                                forwards.close_all("Pause all");
                            }
                            if let Some(links) = &links {
                                links.close_all("Pause all");
                            }
                            // The ears' capture group dies too; resume does
                            // not restart it (the agent asks again).
                            ears.pause();
                        }
                        crate::switch::announce(
                            &outbox,
                            Switch::Pause,
                            switch::on_off(!now_paused),
                            switch::on_off(now_paused),
                            SystemTime::now(),
                        );
                        // The hub hears each toggle once; a resume says the ears stay off.
                        crate::pause::announce(
                            &outbox,
                            now_paused,
                            ears.off_by_pause(),
                            SystemTime::now(),
                        );
                        if let Ok(mut locked) = model.lock() {
                            locked.set_ears_line(Some(ears.line(now_paused)));
                            locked.set_eyes_line(eyes.line(now_paused));
                        }
                        match log.record(&event) {
                            Ok(()) => refresh(&mut tray, &model, &ids, &mut log),
                            Err(error) => {
                                let _noted: Result<(), LogError> =
                                    log.note(&format!("log write failed: {error}"));
                                *control_flow = ControlFlow::Exit;
                            }
                        }
                    }
                    Err(_) => {
                        let _noted: Result<(), LogError> = log
                            .note("tray model lock poisoned; the pause click was ignored");
                    }
                }
            } else if *menu_id == ids.quit {
                // Quit stops the hands first: a running child dies with the app.
                // Then it exits 0 (`ControlFlow::Exit`): a successful exit, after which the
                // LaunchAgent (`KeepAlive = { SuccessfulExit = false }`) leaves Tulpa down.
                paused.set();
                ask_box.deny_all();
                ears.shutdown();
                // The forwards' and serves' ssh children end before the app does.
                if let Some(forwards) = &forwards {
                    forwards.close_all("Quit");
                    forwards.drain(FORWARD_DRAIN);
                }
                if let Some(links) = &links {
                    links.close_all("Quit");
                    links.drain(FORWARD_DRAIN);
                }
                let event = TrayEvent::Quit;
                match log.record(&event) {
                    Ok(()) => {
                        stop.set();
                        let _waited: Result<(), mpsc::RecvTimeoutError> =
                            supervisor_done_rx.recv_timeout(SHUTDOWN_WAIT);
                        *control_flow = ControlFlow::Exit;
                    }
                    Err(error) => {
                        let _noted: Result<(), crate::log::LogError> =
                            log.note(&format!("log write failed: {error}"));
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
        }
    })
}

/// Puts the device pickers on the model: the Mac's devices by name, the ears'
/// picks checked; hidden while the ears are off in config.
fn show_devices(model: &Mutex<TrayModel>, ears: &Ears, devices: &DeviceCache) {
    let lists = ears.is_on().then(|| devices.devices());
    if let Ok(mut locked) = model.lock() {
        let unheard = devices.unheard();
        locked.set_devices(
            lists.as_ref(),
            ears.mic(),
            ears.output(),
            unheard.as_deref(),
        );
    }
}

/// Builds the status item for the current model; the error string is already
/// fit for the activity log.
fn build_tray(model: &Mutex<TrayModel>, ids: &MenuIds) -> Result<tray_icon::TrayIcon, String> {
    let (menu, icon) = {
        let tray_model = model.lock().map_err(|_| {
            "tray model lock poisoned before the status item could be built".to_owned()
        })?;
        let menu = build_menu(&tray_model, ids);
        let icon = tray_icon::Icon::from_rgba(
            render_icon(tray_model.link_state(), tray_model.is_paused(), ICON_SIZE),
            ICON_SIZE,
            ICON_SIZE,
        )
        .map_err(|error| format!("menu bar icon rejected: {error}"))?;
        drop(tray_model);
        (menu, icon)
    };
    tray_icon::TrayIconBuilder::new()
        .with_tooltip("tulpa")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|error| format!("status item failed: {error}"))
}

/// One shown menu row whose text or check mark can change in place.
enum Live {
    Text(muda::MenuItem),
    Check(muda::CheckMenuItem),
    Picker(muda::Submenu, Vec<(EarsMode, muda::CheckMenuItem)>),
    /// A device picker and its check items by device name (`None`: the default).
    Devices(muda::Submenu, Vec<(Option<String>, muda::CheckMenuItem)>),
    Fixed,
}

std::thread_local! {
    /// The rows the shown menu was built from, and handles to update them in place (main thread only, like the menu).
    static SHOWN: std::cell::RefCell<Option<(Vec<MenuEntry>, Vec<Live>)>> = const { std::cell::RefCell::new(None) };
}

/// Updates the shown menu's texts and check marks for `entries` when its layout is the same; false when it isn't
/// (the caller rebuilds). An open menu stays open: nothing is replaced.
fn update_in_place(entries: &[MenuEntry]) -> bool {
    SHOWN.with(|cell| {
        let mut shown = cell.borrow_mut();
        let Some((old, handles)) = shown.as_mut() else {
            return false;
        };
        if !crate::model::same_layout(old, entries) {
            return false;
        }
        for ((entry, row), before) in entries.iter().zip(handles.iter_mut()).zip(old.iter()) {
            // A picker's marks are set again even when nothing changed: muda flips a check item's own
            // mark on a click, so the current item clicked again would otherwise stay unchecked.
            let picker = matches!(
                entry,
                MenuEntry::EarsModes { .. } | MenuEntry::Devices { .. }
            );
            if entry == before && !picker {
                continue;
            }
            match (entry, row) {
                (
                    MenuEntry::Info(line)
                    | MenuEntry::EarsLine(line)
                    | MenuEntry::Forward { line, .. },
                    Live::Text(item),
                ) => {
                    item.set_text(line);
                }
                (
                    MenuEntry::Pause { checked }
                    | MenuEntry::HandsFull { checked }
                    | MenuEntry::LogWindow { open: checked },
                    Live::Check(item),
                ) => {
                    item.set_checked(*checked);
                }
                (MenuEntry::EarsModes { current }, Live::Picker(sub, items)) => {
                    sub.set_text(format!("Ears: {}", current.label()));
                    for (mode, item) in items {
                        item.set_checked(*mode == *current);
                    }
                }
                (
                    MenuEntry::Devices {
                        role,
                        names,
                        current,
                    },
                    Live::Devices(sub, items),
                ) => {
                    sub.set_text(crate::devices::picker_title(*role, current.as_deref()));
                    let rows = crate::devices::picker_rows(*role, names, current.as_deref());
                    if rows
                        .iter()
                        .map(|row| &row.name)
                        .eq(items.iter().map(|(name, _)| name))
                    {
                        // The text too: the same names can mean a pick that came or went
                        // ("(not connected)") when it is the last row.
                        for (row, (_, item)) in rows.iter().zip(items.iter()) {
                            item.set_text(&row.label);
                            item.set_checked(row.checked);
                            item.set_enabled(row.enabled);
                        }
                    } else {
                        // Another list (a device came or went): the items are replaced in place, so an
                        // open menu stays open.
                        while sub.remove_at(0).is_some() {}
                        *items = fill_picker(sub, *role, names, current.as_deref());
                    }
                }
                _ => {}
            }
        }
        *old = entries.to_vec();
        true
    })
}

/// Rebuilds the menu and icon from the current model after a state change; when only texts or check marks changed,
/// the shown menu is updated in place, so an open menu isn't closed.
fn refresh(
    tray: &mut Option<tray_icon::TrayIcon>,
    model: &Mutex<TrayModel>,
    ids: &MenuIds,
    log: &mut ActivityLog,
) {
    let Some(item) = tray.as_mut() else {
        return;
    };
    let Ok(tray_model) = model.lock() else {
        let _noted: Result<(), LogError> =
            log.note("tray model lock poisoned; keeping the previous menu");
        return;
    };
    let entries = tray_model.menu_entries();
    if !update_in_place(&entries) {
        item.set_menu(Some(Box::new(build_menu(&tray_model, ids))));
    }
    if let Ok(icon) = tray_icon::Icon::from_rgba(
        render_icon(tray_model.link_state(), tray_model.is_paused(), ICON_SIZE),
        ICON_SIZE,
        ICON_SIZE,
    ) {
        let _updated: Result<(), tray_icon::Error> = item.set_icon(Some(icon));
    } else {
        let _noted: Result<(), crate::log::LogError> =
            log.note("icon re-render rejected; keeping the previous icon");
    }
}

/// Renders the model as a muda menu; Pause, Quit and a waiting question's
/// Allow / Deny are clickable.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per menu row kind, each a few muda calls; macOS-only, Linux clippy never sees it"
)]
fn build_menu(model: &TrayModel, ids: &MenuIds) -> muda::Menu {
    let menu = muda::Menu::new();
    let entries = model.menu_entries();
    let mut handles: Vec<Live> = Vec::with_capacity(entries.len());
    for entry in entries.clone() {
        match entry {
            MenuEntry::Info(line) => {
                let item = muda::MenuItem::new(line, true, None);
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Text(item));
                continue;
            }
            MenuEntry::EarsLine(line) => {
                let item = muda::MenuItem::with_id(ids.ears_line.clone(), line, true, None);
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Text(item));
                continue;
            }
            MenuEntry::Forward { port, dir, line } => {
                let item = muda::MenuItem::with_id(
                    muda::MenuId::from(crate::forward::close_item(port, dir)),
                    line,
                    true,
                    None,
                );
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Text(item));
                continue;
            }
            MenuEntry::LinkServe { link, line } => {
                let item = muda::MenuItem::with_id(
                    muda::MenuId::from(crate::linkserve::close_item(&link)),
                    line,
                    true,
                    None,
                );
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Text(item));
                continue;
            }
            MenuEntry::Separator => {
                let _appended: muda::Result<()> =
                    menu.append(&muda::PredefinedMenuItem::separator());
            }
            MenuEntry::Pause { checked } => {
                let item = muda::CheckMenuItem::with_id(
                    ids.pause.clone(),
                    "Pause all",
                    true,
                    checked,
                    None,
                );
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Check(item));
                continue;
            }
            MenuEntry::Quit => {
                let _appended: muda::Result<()> = menu.append(&muda::MenuItem::with_id(
                    ids.quit.clone(),
                    "Quit",
                    true,
                    None,
                ));
            }
            MenuEntry::Allow { ask } => {
                let _appended: muda::Result<()> = menu.append(&muda::MenuItem::with_id(
                    crate::askdialog::allow_item(ask),
                    "Allow",
                    true,
                    None,
                ));
            }
            MenuEntry::Deny { ask } => {
                let _appended: muda::Result<()> = menu.append(&muda::MenuItem::with_id(
                    crate::askdialog::deny_item(ask),
                    "Deny",
                    true,
                    None,
                ));
            }
            MenuEntry::EarsModes { current } => {
                let picker = muda::Submenu::new(format!("Ears: {}", current.label()), true);
                let mut items = Vec::new();
                for mode in EarsMode::ALL {
                    let item = muda::CheckMenuItem::with_id(
                        format!("{EARS_MODE_PREFIX}{}", mode.id()),
                        mode.label(),
                        true,
                        mode == current,
                        None,
                    );
                    let _appended: muda::Result<()> = picker.append(&item);
                    items.push((mode, item));
                }
                let _appended: muda::Result<()> = menu.append(&picker);
                handles.push(Live::Picker(picker, items));
                continue;
            }
            MenuEntry::HandsFull { checked } => {
                let item = muda::CheckMenuItem::with_id(
                    ids.hands_full.clone(),
                    "Hands: full",
                    true,
                    checked,
                    None,
                );
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Check(item));
                continue;
            }
            MenuEntry::OpenConfig => {
                let _appended: muda::Result<()> = menu.append(&muda::MenuItem::with_id(
                    ids.open_config.clone(),
                    "Open config",
                    true,
                    None,
                ));
            }
            MenuEntry::LogWindow { open } => {
                let item = muda::CheckMenuItem::with_id(
                    ids.log_window.clone(),
                    "Log window",
                    true,
                    open,
                    None,
                );
                let _appended: muda::Result<()> = menu.append(&item);
                handles.push(Live::Check(item));
                continue;
            }
            MenuEntry::Devices {
                role,
                names,
                current,
            } => {
                let picker = muda::Submenu::new(
                    crate::devices::picker_title(role, current.as_deref()),
                    true,
                );
                let items = fill_picker(&picker, role, &names, current.as_deref());
                let _appended: muda::Result<()> = menu.append(&picker);
                handles.push(Live::Devices(picker, items));
                continue;
            }
        }
        handles.push(Live::Fixed);
    }
    SHOWN.with(|cell| *cell.borrow_mut() = Some((entries, handles)));
    menu
}

/// Fills a device picker: the default, a separator, then each device by name;
/// returns its check items by device (`None`: the default).
fn fill_picker(
    picker: &muda::Submenu,
    role: DeviceRole,
    names: &[String],
    current: Option<&str>,
) -> Vec<(Option<String>, muda::CheckMenuItem)> {
    let mut items = Vec::new();
    for (index, row) in crate::devices::picker_rows(role, names, current)
        .into_iter()
        .enumerate()
    {
        if index == 1 {
            let _appended: muda::Result<()> = picker.append(&muda::PredefinedMenuItem::separator());
        }
        let item = muda::CheckMenuItem::with_id(
            crate::devices::item_id(role, row.name.as_deref()),
            &row.label,
            row.enabled,
            row.checked,
            None,
        );
        let _appended: muda::Result<()> = picker.append(&item);
        items.push((row.name, item));
    }
    items
}
