//! The taskbar jump list (T14.71): the menu of the taskbar button's right-click holds the tray's
//! meeting item and "Quit Earshot", each a start of this exe with one fixed argument. The
//! single-instance plugin hands that start's arguments to the running app, which acts through
//! the tray's own items ([`TrayItem`], dispatched by [`crate::shell::item_handler`] for the tray
//! menu and the jump list alike: [`crate::shell::Shell::toggle`], which goes through
//! [`crate::shell::press`], and [`crate::shell::Shell::quit`]). With no Earshot running such a start exits at once and starts
//! nothing ([`early_exit`], first in `main`): a recording started from there would run with its
//! window hidden.
//!
//! The list itself is set through COM on a thread of its own (`jumplist_win.rs`), woken by
//! [`JumpBoard::wake`] whenever the tray is drawn. The thread reads the phase itself after a
//! burst of wake-ups has settled ([`follow`]), so the last list it sets is the phase as it is,
//! whatever order the wake-ups came in. On a clean exit the list is deleted
//! ([`JumpBoard::clear`]), so a pinned button with Earshot closed offers no dead entries; the next
//! start sets it again. This module is the portable part.

use std::ffi::OsString;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::shell::{Phase, QUIT_LABEL, tray_view};

/// The argument of the jump list's meeting entry. No child mode, the replay or `--autostart`
/// starts with `--taskbar-`, and the argument must be the process's only one, so neither a
/// child's command line nor the `Run` value can be read as it (pinned by a test).
pub const MEETING_ARG: &str = "--taskbar-meeting";
/// The argument of the jump list's "Quit Earshot" entry.
pub const QUIT_ARG: &str = "--taskbar-quit";

/// What a jump list entry asks the running app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpAction {
    /// What the tray's meeting item does now (start, nothing while starting, or stop).
    Meeting,
    /// What the tray's "Quit Earshot" does.
    Quit,
}

impl JumpAction {
    /// The entry's argument.
    #[must_use]
    pub fn arg(self) -> &'static str {
        match self {
            Self::Meeting => MEETING_ARG,
            Self::Quit => QUIT_ARG,
        }
    }

    /// The tray item this entry is.
    #[must_use]
    pub fn tray_item(self) -> TrayItem {
        match self {
            Self::Meeting => TrayItem::Toggle,
            Self::Quit => TrayItem::Quit,
        }
    }

    /// The action `args` (the process's arguments after the program) ask for: exactly one
    /// argument, one of the two. Anything else is not a jump list start.
    #[must_use]
    pub fn of(args: &[OsString]) -> Option<Self> {
        match args {
            [only] if only == MEETING_ARG => Some(Self::Meeting),
            [only] if only == QUIT_ARG => Some(Self::Quit),
            _ => None,
        }
    }

    /// The action of a second start's arguments as the single-instance plugin hands them over
    /// (the program first).
    #[must_use]
    pub fn of_forwarded(args: &[String]) -> Option<Self> {
        let rest: Vec<OsString> = args.iter().skip(1).map(OsString::from).collect();
        Self::of(&rest)
    }
}

/// The exit code for a jump list start that finds no Earshot running (`running` asks only
/// then), or `None` to go on. Called first in `main`, before Tauri, the store or a window.
pub fn early_exit(args: &[OsString], running: impl FnOnce() -> bool) -> Option<i32> {
    JumpAction::of(args)
        .is_some()
        .then(running)
        .and_then(|running| (!running).then_some(0))
}

/// The exit code for a jump list start that reached the app's setup: it is then the only
/// Earshot (the one it was meant for ended between [`early_exit`] and the single-instance
/// plugin), and it starts nothing either. `None` for any other start.
#[must_use]
pub fn exit_in_setup(args: &[OsString]) -> Option<i32> {
    JumpAction::of(args).map(|_| 0)
}

/// An item of the tray's menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayItem {
    /// "Open Earshot".
    Open,
    /// The meeting item ("Start meeting", "Stop meeting").
    Toggle,
    /// "Quit Earshot".
    Quit,
}

impl TrayItem {
    /// Every item, in the menu's order.
    pub const ALL: [Self; 3] = [Self::Open, Self::Toggle, Self::Quit];

    /// The menu item's id.
    #[must_use]
    pub fn menu_id(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Toggle => "toggle",
            Self::Quit => "quit",
        }
    }

    /// The item of a menu event's id.
    #[must_use]
    pub fn of_menu_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.menu_id() == id)
    }
}

/// The tray item a second start with `args` (the program first) is: a jump list entry's own
/// item, any other start "Open Earshot" (as before T14.71). The shell does it through the one
/// dispatch the tray menu uses ([`crate::shell::item_handler`]).
#[must_use]
pub fn item_of_second_start(args: &[String]) -> TrayItem {
    JumpAction::of_forwarded(args).map_or(TrayItem::Open, JumpAction::tray_item)
}

/// One entry of the jump list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JumpEntry {
    /// What the menu shows: the tray's own words.
    pub title: &'static str,
    /// What it does.
    pub action: JumpAction,
}

/// The jump list in `phase`, top first: the tray's meeting item as the tray shows it, then
/// "Quit Earshot". A jump list entry cannot be greyed out, so while a meeting starts it reads
/// as the tray's item does and a press does nothing, as [`crate::shell::press`] decides.
#[must_use]
pub fn entries(phase: Phase) -> [JumpEntry; 2] {
    [
        JumpEntry {
            title: tray_view(phase).toggle_label,
            action: JumpAction::Meeting,
        },
        JumpEntry {
            title: QUIT_LABEL,
            action: JumpAction::Quit,
        },
    ]
}

/// What the jump list's thread is told.
pub enum JumpMsg {
    /// The tray was drawn: read the phase and set the list if it differs.
    Wake,
    /// The app is quitting: delete the list, answer, and set nothing more.
    Clear(Sender<()>),
}

/// How long the thread waits for the next wake-up before it reads the phase: wake-ups closer
/// together than this set the list once (a judgement; a phase lasts far longer).
pub const SETTLE: Duration = Duration::from_millis(200);

/// What the thread does next.
enum Next {
    Draw,
    Clear(Sender<()>),
    Stop,
}

/// The next step: a burst of wake-ups settles into one draw; a clear goes at once.
fn next(rx: &Receiver<JumpMsg>) -> Next {
    match rx.recv() {
        Err(_) => return Next::Stop,
        Ok(JumpMsg::Clear(reply)) => return Next::Clear(reply),
        Ok(JumpMsg::Wake) => {}
    }
    loop {
        match rx.recv_timeout(SETTLE) {
            Ok(JumpMsg::Wake) => {}
            Ok(JumpMsg::Clear(reply)) => return Next::Clear(reply),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return Next::Draw,
        }
    }
}

/// The jump list thread's loop. Each draw reads the phase with `read` at that moment, so the last
/// list set is the phase as it is, whatever order the wake-ups came in; `set` says whether the
/// shell took the list (a refusal is tried again at the next wake-up). `clear` deletes the list
/// once, on [`JumpMsg::Clear`], after which nothing is set again.
pub fn follow(
    rx: &Receiver<JumpMsg>,
    read: &dyn Fn() -> Phase,
    mut set: impl FnMut(&[JumpEntry; 2]) -> bool,
    clear: impl FnOnce(),
) {
    let mut shown: Option<[JumpEntry; 2]> = None;
    loop {
        match next(rx) {
            Next::Draw => {
                let want = entries(read());
                if shown != Some(want) {
                    shown = set(&want).then_some(want);
                }
            }
            Next::Clear(reply) => {
                clear();
                let _ = reply.send(());
                return;
            }
            Next::Stop => return,
        }
    }
}

/// Where the jump list is drawn: the COM thread in the app, a fake in tests.
pub trait JumpSurface: Send + Sync {
    /// The tray was drawn. Never blocks.
    fn wake(&self);
    /// Delete the list; the receiver hears when it is done. Never blocks.
    fn clear(&self) -> Option<Receiver<()>>;
}

/// The jump list of this process, once started.
#[derive(Default)]
pub struct JumpBoard {
    surface: OnceLock<Box<dyn JumpSurface>>,
}

impl JumpBoard {
    /// The list exists: wake `surface` from now on.
    pub fn set(&self, surface: Box<dyn JumpSurface>) {
        let _ = self.surface.set(surface);
    }

    /// The tray was drawn.
    pub fn wake(&self) {
        if let Some(surface) = self.surface.get() {
            surface.wake();
        }
    }

    /// Starts deleting the list (a clean exit).
    #[must_use]
    pub fn clear(&self) -> Option<Receiver<()>> {
        self.surface.get().and_then(|surface| surface.clear())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn jumplist_each_argument_is_its_action_and_nothing_else_is() {
        assert_eq!(JumpAction::of(&os(&[QUIT_ARG])), Some(JumpAction::Quit));
        assert_eq!(
            JumpAction::of(&os(&[MEETING_ARG])),
            Some(JumpAction::Meeting)
        );
        assert_eq!(JumpAction::Quit.arg(), QUIT_ARG);
        assert_eq!(JumpAction::Meeting.arg(), MEETING_ARG);
        // Not a jump list start: nothing, another argument, an unknown one, a near miss, or the
        // argument beside another.
        for args in [
            &[][..],
            &[crate::shell::AUTOSTART_ARG][..],
            &["--taskbar-unknown"][..],
            &["--taskbar-quit=1"][..],
            &["--TASKBAR-QUIT"][..],
            &[QUIT_ARG, MEETING_ARG][..],
            &[crate::shell::AUTOSTART_ARG, QUIT_ARG][..],
        ] {
            assert_eq!(JumpAction::of(&os(args)), None, "{args:?}");
        }
        // The plugin hands over the program first.
        let forwarded = |rest: &[&str]| {
            let mut all = vec![r"C:\Apps\Earshot\earshot.exe".to_owned()];
            all.extend(rest.iter().map(|s| (*s).to_owned()));
            JumpAction::of_forwarded(&all)
        };
        assert_eq!(forwarded(&[QUIT_ARG]), Some(JumpAction::Quit));
        assert_eq!(forwarded(&[MEETING_ARG]), Some(JumpAction::Meeting));
        assert_eq!(forwarded(&[]), None);
        assert_eq!(forwarded(&["--elsewhere"]), None);
        assert_eq!(JumpAction::of_forwarded(&[]), None);
    }

    #[test]
    fn jumplist_arguments_reach_no_child_mode_or_the_replay() {
        for arg in [QUIT_ARG, MEETING_ARG] {
            let args = os(&[arg]);
            assert_eq!(crate::child_modes::run(&args), None, "{arg}");
            assert!(!crate::headless::asked(&args), "{arg}");
            assert!(!crate::shell::started_by_windows(&args), "{arg}");
        }
    }

    #[test]
    fn jumplist_a_start_with_no_app_running_exits_before_anything_starts() {
        // No app: exit at once.
        assert_eq!(early_exit(&os(&[QUIT_ARG]), || false), Some(0));
        assert_eq!(early_exit(&os(&[MEETING_ARG]), || false), Some(0));
        // An app running: go on, so the plugin hands the arguments over.
        assert_eq!(early_exit(&os(&[QUIT_ARG]), || true), None);
        assert_eq!(early_exit(&os(&[MEETING_ARG]), || true), None);
        // Any other start never asks.
        for args in [&[][..], &[crate::shell::AUTOSTART_ARG][..], &["--x"][..]] {
            assert_eq!(early_exit(&os(args), || panic!("asked for {args:?}")), None);
        }
    }

    #[test]
    fn jumplist_a_start_that_reaches_setup_exits_there() {
        assert_eq!(exit_in_setup(&os(&[QUIT_ARG])), Some(0));
        assert_eq!(exit_in_setup(&os(&[MEETING_ARG])), Some(0));
        // Control: the app's own starts go on.
        assert_eq!(exit_in_setup(&os(&[])), None);
        assert_eq!(exit_in_setup(&os(&[crate::shell::AUTOSTART_ARG])), None);
    }

    fn item(rest: &[&str]) -> TrayItem {
        let mut args = vec!["earshot.exe".to_owned()];
        args.extend(rest.iter().map(|s| (*s).to_owned()));
        item_of_second_start(&args)
    }

    #[test]
    fn jumplist_a_second_start_is_quit_the_meeting_item_or_open() {
        assert_eq!(item(&[QUIT_ARG]), TrayItem::Quit);
        assert_eq!(item(&[MEETING_ARG]), TrayItem::Toggle);
        // As before T14.71: any other second start opens the window.
        assert_eq!(item(&[]), TrayItem::Open);
        assert_eq!(item(&["--taskbar-unknown"]), TrayItem::Open);
        assert_eq!(item(&[crate::shell::AUTOSTART_ARG]), TrayItem::Open);
    }

    #[test]
    fn jumplist_tray_menu_ids_are_their_items() {
        for item in TrayItem::ALL {
            assert_eq!(TrayItem::of_menu_id(item.menu_id()), Some(item));
        }
        assert_eq!(TrayItem::of_menu_id("toggle"), Some(TrayItem::Toggle));
        assert_eq!(TrayItem::of_menu_id("quit"), Some(TrayItem::Quit));
        assert_eq!(TrayItem::of_menu_id("open"), Some(TrayItem::Open));
        assert_eq!(TrayItem::of_menu_id("other"), None);
    }

    /// Runs [`follow`] over `msgs` (then the senders gone), reading the phase from `phase` at
    /// each draw; returns every list set and whether the list was cleared.
    fn run(msgs: Vec<JumpMsg>, phase: &Mutex<Phase>) -> (Vec<[JumpEntry; 2]>, bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        for m in msgs {
            tx.send(m).expect("send");
        }
        drop(tx);
        let mut set = Vec::new();
        let mut cleared = false;
        follow(
            &rx,
            &|| *phase.lock().expect("phase"),
            |want| {
                set.push(*want);
                true
            },
            || cleared = true,
        );
        (set, cleared)
    }

    #[test]
    fn jumplist_wake_ups_in_any_order_end_on_the_phase_read_at_draw_time() {
        // Two threads changed the phase (Recording, then Idle) and their wake-ups arrive in any
        // order, the older one last: the list is set from the phase as it is when drawn.
        let phase = Mutex::new(Phase::Idle);
        let (set, cleared) = run(vec![JumpMsg::Wake, JumpMsg::Wake, JumpMsg::Wake], &phase);
        assert_eq!(set, [entries(Phase::Idle)]);
        assert!(!cleared);
        // Control: the same wake-ups with the phase at Recording set Recording's list.
        let phase = Mutex::new(Phase::Recording);
        let (set, _) = run(vec![JumpMsg::Wake, JumpMsg::Wake], &phase);
        assert_eq!(set, [entries(Phase::Recording)]);
    }

    #[test]
    fn jumplist_a_change_back_is_set_again() {
        let phase = Arc::new(Mutex::new(Phase::Recording));
        let (tx, rx) = std::sync::mpsc::channel();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let read = Arc::clone(&phase);
        let thread = std::thread::spawn(move || {
            follow(
                &rx,
                &|| *read.lock().expect("phase"),
                |want| {
                    let _ = seen_tx.send(*want);
                    true
                },
                || {},
            );
        });
        let wait = Duration::from_secs(10);
        tx.send(JumpMsg::Wake).expect("send");
        assert_eq!(seen_rx.recv_timeout(wait), Ok(entries(Phase::Recording)));
        *phase.lock().expect("phase") = Phase::Idle;
        tx.send(JumpMsg::Wake).expect("send");
        assert_eq!(seen_rx.recv_timeout(wait), Ok(entries(Phase::Idle)));
        // An unchanged phase sets nothing.
        tx.send(JumpMsg::Wake).expect("send");
        assert!(seen_rx.recv_timeout(SETTLE * 3).is_err());
        drop(tx);
        thread.join().expect("join");
    }

    #[test]
    fn jumplist_a_clear_deletes_once_answers_and_sets_nothing_more() {
        let phase = Mutex::new(Phase::Idle);
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let (set, cleared) = run(
            vec![JumpMsg::Wake, JumpMsg::Clear(reply_tx), JumpMsg::Wake],
            &phase,
        );
        // The clear came inside the burst: no list is set, before or after it.
        assert!(set.is_empty(), "{set:?}");
        assert!(cleared);
        assert_eq!(reply_rx.try_recv(), Ok(()));
        // Control: without the clear the list is set and not deleted.
        let (set, cleared) = run(vec![JumpMsg::Wake], &phase);
        assert_eq!(set, [entries(Phase::Idle)]);
        assert!(!cleared);
    }

    #[derive(Default)]
    struct Surface {
        wakes: AtomicUsize,
    }

    impl JumpSurface for Arc<Surface> {
        fn wake(&self) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
        fn clear(&self) -> Option<Receiver<()>> {
            None
        }
    }

    #[test]
    fn jumplist_a_tray_redraw_wakes_the_jump_list() {
        let surface = Arc::new(Surface::default());
        let board = JumpBoard::default();
        // No list yet: nothing to wake.
        crate::shell::refresh_surfaces(&crate::shell::TrayBoard::default(), &board, || Phase::Idle);
        board.set(Box::new(Arc::clone(&surface)));
        assert_eq!(surface.wakes.load(Ordering::SeqCst), 0, "control");
        crate::shell::refresh_surfaces(&crate::shell::TrayBoard::default(), &board, || {
            Phase::Recording
        });
        assert_eq!(surface.wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn jumplist_entries_follow_the_tray() {
        for phase in [
            Phase::Idle,
            Phase::Starting,
            Phase::Recording,
            Phase::Finishing,
        ] {
            let [meeting, quit] = entries(phase);
            assert_eq!(meeting.title, tray_view(phase).toggle_label, "{phase:?}");
            assert_eq!(meeting.action, JumpAction::Meeting);
            assert_eq!(quit.title, QUIT_LABEL);
            assert_eq!(quit.action, JumpAction::Quit);
        }
        assert_eq!(entries(Phase::Idle)[0].title, "Start meeting");
        assert_eq!(entries(Phase::Recording)[0].title, "Stop meeting");
        // Control: the entry changes with the phase.
        assert_ne!(entries(Phase::Idle)[0], entries(Phase::Recording)[0]);
        assert_eq!(QUIT_LABEL, "Quit Earshot");
    }
}
