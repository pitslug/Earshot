//! The taskbar jump list (T14.71): the menu of the taskbar button's right-click holds the tray's
//! meeting item and "Quit Earshot", each a start of this exe with one fixed argument. The
//! single-instance plugin hands that start's arguments to the running app, which acts through
//! the tray's own paths ([`crate::shell::Shell::toggle`], which goes through
//! [`crate::shell::press`], and [`crate::shell::Shell::quit`]). With no Earshot running such a
//! start exits at once and starts nothing ([`early_exit`], first in `main`): a recording started
//! from there would run with its window hidden.
//!
//! The list itself is set through COM on a thread of its own (`jumplist_win.rs`); this module is
//! the portable part: the arguments, what a second start does, and the entries for a phase.

use std::ffi::OsString;

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

/// What a second start of Earshot can ask of the running one.
pub trait SecondStart {
    /// Open the window (any start that is not a jump list entry).
    fn show_window(&self);
    /// The tray's meeting item.
    fn meeting(&self);
    /// The tray's "Quit Earshot".
    fn quit(&self);
}

/// The running app heard a second start with `args` (the program first).
pub fn second_start(args: &[String], to: &dyn SecondStart) {
    match JumpAction::of_forwarded(args) {
        Some(JumpAction::Meeting) => to.meeting(),
        Some(JumpAction::Quit) => to.quit(),
        None => to.show_window(),
    }
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

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

    #[derive(Default)]
    struct Heard(RefCell<Vec<&'static str>>);

    impl SecondStart for Heard {
        fn show_window(&self) {
            self.0.borrow_mut().push("window");
        }
        fn meeting(&self) {
            self.0.borrow_mut().push("meeting");
        }
        fn quit(&self) {
            self.0.borrow_mut().push("quit");
        }
    }

    fn heard(rest: &[&str]) -> Vec<&'static str> {
        let mut args = vec!["earshot.exe".to_owned()];
        args.extend(rest.iter().map(|s| (*s).to_owned()));
        let to = Heard::default();
        second_start(&args, &to);
        to.0.into_inner()
    }

    #[test]
    fn jumplist_a_second_start_goes_to_quit_the_meeting_item_or_the_window() {
        assert_eq!(heard(&[QUIT_ARG]), ["quit"]);
        assert_eq!(heard(&[MEETING_ARG]), ["meeting"]);
        // As before T14.71: any other second start opens the window.
        assert_eq!(heard(&[]), ["window"]);
        assert_eq!(heard(&["--taskbar-unknown"]), ["window"]);
        assert_eq!(heard(&[crate::shell::AUTOSTART_ARG]), ["window"]);
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
