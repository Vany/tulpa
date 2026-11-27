//! The owner's Allow/Deny dialog: decided here, drawn by a presenter.
//!
//! The hands' questions and the eyes' camera question wait in the
//! [`AskBox`](crate::hands::AskBox) in the order asked. This module decides,
//! with no platform code, what the dialog shows and when it goes:
//!
//! - The first question in line is shown, one dialog at a time. A newer
//!   question replaces nothing; it waits its turn.
//! - **Allow** is clickable only [`ARM_DELAY`] after the dialog appeared, so a
//!   click aimed at something else as it popped up cannot allow. **Deny** is
//!   the default button, which Return presses.
//! - The dialog closes when its question is answered (in the dialog, from the
//!   menu, or denied by Pause or Quit), or when the question's own time is up.
//!   The engine then refuses it as expired, and no dialog stays up after the
//!   engine gave up.
//! - Then the next question in line is shown.
//!
//! The presenter only draws: an alert on the Mac (`macos.rs`), a recorder in
//! the tests. A click in the dialog reaches the box as
//! [`AskBox::answer`](crate::hands::AskBox::answer) with the question's
//! number, as a click in the menu does.

use std::time::{Duration, Instant};

use tulpa_hands::AskAnswer;

use crate::hands::Asked;

/// How long after the dialog appears its **Allow** becomes clickable.
pub const ARM_DELAY: Duration = Duration::from_secs(1);

/// The dialog's buttons in the order they are added. The first is the
/// default, which Return presses: **Deny**.
pub const BUTTONS: [(&str, AskAnswer); 2] =
    [("Deny", AskAnswer::Deny), ("Allow", AskAnswer::Allow)];

/// The menu's Allow item ids: this prefix and the question's number.
const ALLOW_ITEM: &str = "tulpa.allow.";
/// The menu's Deny item ids: this prefix and the question's number.
const DENY_ITEM: &str = "tulpa.deny.";

/// What the button at `index` answers, in [`BUTTONS`] order.
///
/// `None` for anything else: the dialog closed without a click on a button,
/// because Tulpa closed it (its question answered elsewhere, or expired).
#[must_use]
pub fn button_answer(index: isize) -> Option<AskAnswer> {
    usize::try_from(index)
        .ok()
        .and_then(|index| BUTTONS.get(index))
        .map(|(_, answer)| *answer)
}

/// The menu's Allow item for question `ask`.
#[must_use]
pub fn allow_item(ask: u64) -> String {
    format!("{ALLOW_ITEM}{ask}")
}

/// The menu's Deny item for question `ask`.
#[must_use]
pub fn deny_item(ask: u64) -> String {
    format!("{DENY_ITEM}{ask}")
}

/// The question and the answer a menu item stands for, when it is an Allow
/// or a Deny item.
#[must_use]
pub fn menu_answer(item: &str) -> Option<(u64, AskAnswer)> {
    let (number, answer) = item
        .strip_prefix(ALLOW_ITEM)
        .map(|number| (number, AskAnswer::Allow))
        .or_else(|| {
            item.strip_prefix(DENY_ITEM)
                .map(|number| (number, AskAnswer::Deny))
        })?;
    // Digits only: `parse` would also take a leading `+`.
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok().map(|ask| (ask, answer))
}

/// What the dialog says.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DialogText {
    /// The first line, in bold.
    pub title: String,
    /// The question, and what no answer does.
    pub body: String,
}

/// The dialog's text for `ask`, shown at `now`.
#[must_use]
pub fn dialog_text(ask: &Asked, now: Instant) -> DialogText {
    let left = ask.deadline.saturating_duration_since(now);
    // Rounded up: a question shown with 29.6 s left says 30 s.
    let seconds = left
        .as_secs()
        .saturating_add(u64::from(left.subsec_nanos() > 0));
    DialogText {
        title: "The agent asks".to_owned(),
        body: format!(
            "{}\n\nAsked at {}. With no answer within {seconds} s, it is refused.",
            ask.what, ask.at
        ),
    }
}

/// The notification posted with a new dialog. It only points at the dialog:
/// a click on the notification answers nothing.
#[must_use]
pub fn notice(ask: &Asked) -> String {
    format!("{} — answer in the Tulpa dialog", ask.what)
}

/// Draws the dialog: an alert on the Mac, a recorder in the tests.
pub trait Presenter {
    /// Puts up the dialog for `ask` with `text`, its **Allow** not yet
    /// clickable. Never called while another dialog is up.
    fn show(&mut self, ask: &Asked, text: &DialogText);
    /// Makes **Allow** clickable in the dialog of question `id`.
    fn arm(&mut self, id: u64);
    /// Takes down the dialog of question `id`: its question was answered or
    /// expired. Harmless when the dialog went already (its own click closed
    /// it).
    fn close(&mut self, id: u64);
}

/// Why [`AskDialog::sync`] closed the dialog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Closed {
    /// Its question left the line: answered in the dialog or the menu,
    /// denied by Pause or Quit, or refused as expired by the engine's own
    /// clock before this one saw the deadline.
    Gone(u64),
    /// Its question's time is up and it is still in line: the engine is about
    /// to refuse it as expired.
    Expired(u64),
}

/// The question the dialog shows.
#[derive(Clone, Copy, Debug)]
struct Shown {
    id: u64,
    deadline: Instant,
    /// When **Allow** becomes clickable; `None` once it is.
    arm_at: Option<Instant>,
}

/// Decides what the dialog shows, one question at a time; the presenter's
/// event loop drives it with [`AskDialog::sync`].
#[derive(Debug)]
pub struct AskDialog<P> {
    presenter: P,
    shown: Option<Shown>,
}

impl<P: Presenter> AskDialog<P> {
    /// A dialog with nothing shown, drawn by `presenter`.
    pub const fn new(presenter: P) -> Self {
        Self {
            presenter,
            shown: None,
        }
    }

    /// Brings the dialog in line with `first`, the question first in line,
    /// at `now`. Call it when the box changes and when
    /// [`AskDialog::next_wake`] comes.
    ///
    /// The shown dialog closes when its question is no longer first in line
    /// or its time is up; otherwise its **Allow** is armed once
    /// [`ARM_DELAY`] has passed. With no dialog up, the first question is
    /// shown while it still has time. Returns why a dialog closed, if one
    /// did.
    pub fn sync(&mut self, first: Option<&Asked>, now: Instant) -> Option<Closed> {
        let mut closed = None;
        if let Some(shown) = self.shown {
            if first.is_none_or(|asked| asked.id != shown.id) {
                closed = Some(Closed::Gone(shown.id));
            } else if now >= shown.deadline {
                closed = Some(Closed::Expired(shown.id));
            } else if shown.arm_at.is_some_and(|at| now >= at) {
                self.presenter.arm(shown.id);
                self.shown = Some(Shown {
                    arm_at: None,
                    ..shown
                });
            }
            if closed.is_some() {
                self.presenter.close(shown.id);
                self.shown = None;
            }
        }
        if self.shown.is_none()
            && let Some(asked) = first
            && now < asked.deadline
        {
            self.presenter.show(asked, &dialog_text(asked, now));
            self.shown = Some(Shown {
                id: asked.id,
                deadline: asked.deadline,
                // An Instant this far out never comes: then Allow stays off.
                arm_at: Some(now.checked_add(ARM_DELAY).unwrap_or(asked.deadline)),
            });
        }
        closed
    }

    /// When the event loop must wake next: to arm **Allow**, or to close the
    /// dialog at its question's deadline. `None` while no dialog is up.
    #[must_use]
    pub fn next_wake(&self) -> Option<Instant> {
        self.shown.map(|shown| {
            shown
                .arm_at
                .map_or(shown.deadline, |at| at.min(shown.deadline))
        })
    }

    /// The number of the question the dialog shows, if one is up.
    #[must_use]
    pub fn shown(&self) -> Option<u64> {
        self.shown.map(|shown| shown.id)
    }

    /// The presenter.
    pub const fn presenter(&self) -> &P {
        &self.presenter
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use tulpa_hands::Asker;

    use super::*;
    use crate::hands::AskBox;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    /// A presenter that writes down what it was told to draw.
    #[derive(Debug, Default)]
    struct Recorder {
        calls: Vec<String>,
    }

    impl Presenter for Recorder {
        fn show(&mut self, ask: &Asked, text: &DialogText) {
            self.calls
                .push(format!("show {}: {} | {}", ask.id, text.title, text.body));
        }

        fn arm(&mut self, id: u64) {
            self.calls.push(format!("arm {id}"));
        }

        fn close(&mut self, id: u64) {
            self.calls.push(format!("close {id}"));
        }
    }

    /// What the recorder was told, without the shown texts.
    fn calls(dialog: &AskDialog<Recorder>) -> Vec<String> {
        dialog
            .presenter()
            .calls
            .iter()
            .map(|call| call.split(':').next().unwrap_or_default().to_owned())
            .collect()
    }

    /// Asks `what` on a thread of its own, as the engine does.
    fn asking(ask_box: &Arc<AskBox>, what: &str, timeout: Duration) -> JoinHandle<AskAnswer> {
        let asker = Arc::clone(ask_box);
        let what = what.to_owned();
        thread::spawn(move || asker.ask(&what, timeout))
    }

    /// The question first in line once `ready` holds of it (up to 5 s).
    fn first_when(ask_box: &AskBox, ready: impl Fn(&Asked) -> bool) -> Result<Asked> {
        for _ in 0..500 {
            if let Some(first) = ask_box.head().filter(&ready) {
                return Ok(first);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err("the question never came".into())
    }

    /// When `asked`, asked with `timeout`, was asked: its deadline less that.
    fn asked_at(asked: &Asked, timeout: Duration) -> Result<Instant> {
        Ok(asked
            .deadline
            .checked_sub(timeout)
            .ok_or("an instant before the deadline")?)
    }

    #[test]
    fn a_new_ask_shows_one_dialog() -> Result<()> {
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let mut dialog = AskDialog::new(Recorder::default());
        assert_eq!(dialog.sync(None, Instant::now()), None);
        assert_eq!(dialog.next_wake(), None, "nothing to wake for");

        let waiter = asking(&ask_box, "read ~/w/notes.txt", Duration::from_secs(30));
        let first = first_when(&ask_box, |_| true)?;
        // Shown the moment it was asked: the dialog goes by the times it is
        // given, not by how late this thread ran.
        let shown_at = asked_at(&first, Duration::from_secs(30))?;
        assert_eq!(dialog.sync(Some(&first), shown_at), None);
        assert_eq!(dialog.shown(), Some(first.id));
        let shown = dialog
            .presenter()
            .calls
            .first()
            .cloned()
            .unwrap_or_default();
        assert!(
            shown.contains(": The agent asks | read ~/w/notes.txt\n\nAsked at ")
                && shown.ends_with(". With no answer within 30 s, it is refused."),
            "{shown}"
        );
        // The box changing again, or the loop waking early, shows nothing more.
        dialog.sync(ask_box.head().as_ref(), shown_at);
        dialog.sync(ask_box.head().as_ref(), shown_at + ARM_DELAY / 2);
        assert_eq!(calls(&dialog), [format!("show {}", first.id)]);

        assert_eq!(
            dialog.next_wake(),
            Some(shown_at + ARM_DELAY),
            "wakes to arm Allow"
        );
        dialog.sync(ask_box.head().as_ref(), shown_at + ARM_DELAY);
        assert_eq!(
            calls(&dialog),
            [format!("show {}", first.id), format!("arm {}", first.id)]
        );
        assert_eq!(
            dialog.next_wake(),
            Some(first.deadline),
            "then for the deadline"
        );

        ask_box.deny_all();
        assert_eq!(waiter.join().ok(), Some(AskAnswer::Deny));
        assert_eq!(
            dialog.sync(ask_box.head().as_ref(), Instant::now()),
            Some(Closed::Gone(first.id))
        );
        assert_eq!(calls(&dialog).last(), Some(&format!("close {}", first.id)));
        assert_eq!(dialog.next_wake(), None);
        Ok(())
    }

    #[test]
    fn its_timeout_closes_it_as_expired() -> Result<()> {
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let mut dialog = AskDialog::new(Recorder::default());
        let waiter = asking(
            &ask_box,
            "take one photo with the camera",
            Duration::from_secs(2),
        );
        let first = first_when(&ask_box, |_| true)?;
        // Shown half a second before its deadline, which then comes before
        // Allow would be armed.
        let shown_at = first
            .deadline
            .checked_sub(Duration::from_millis(500))
            .ok_or("an instant before the deadline")?;
        dialog.sync(Some(&first), shown_at);
        assert_eq!(
            dialog.next_wake(),
            Some(first.deadline),
            "the deadline comes first"
        );

        // The loop wakes at the deadline, maybe before the engine's own clock
        // took the question out of the line: the dialog goes all the same,
        // and is not shown again.
        assert_eq!(
            dialog.sync(Some(&first), first.deadline),
            Some(Closed::Expired(first.id))
        );
        assert_eq!(dialog.sync(Some(&first), first.deadline), None);
        assert_eq!(
            calls(&dialog),
            [format!("show {}", first.id), format!("close {}", first.id)]
        );
        assert_eq!(dialog.next_wake(), None);

        assert_eq!(
            waiter.join().ok(),
            Some(AskAnswer::Expired),
            "the engine's own clock"
        );
        assert!(ask_box.head().is_none());
        assert!(
            !ask_box.answer(first.id, AskAnswer::Allow),
            "a click after the deadline answers nothing"
        );
        assert_eq!(dialog.sync(None, Instant::now()), None);
        assert_eq!(calls(&dialog).len(), 2);
        Ok(())
    }

    #[test]
    fn an_answer_from_the_menu_closes_the_dialog() -> Result<()> {
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let mut dialog = AskDialog::new(Recorder::default());
        let waiter = asking(&ask_box, "put 12 B into ~/w/a.txt", Duration::from_secs(30));
        let first = first_when(&ask_box, |_| true)?;
        dialog.sync(Some(&first), Instant::now());

        // The menu's Allow item carries the question's number.
        let (ask, answer) = menu_answer(&allow_item(first.id)).ok_or("an Allow item")?;
        assert_eq!((ask, answer), (first.id, AskAnswer::Allow));
        assert!(ask_box.answer(ask, answer));
        assert_eq!(waiter.join().ok(), Some(AskAnswer::Allow));

        assert_eq!(
            dialog.sync(ask_box.head().as_ref(), Instant::now()),
            Some(Closed::Gone(first.id))
        );
        assert_eq!(
            calls(&dialog),
            [format!("show {}", first.id), format!("close {}", first.id)]
        );
        Ok(())
    }

    #[test]
    fn two_asks_show_in_order() -> Result<()> {
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let mut dialog = AskDialog::new(Recorder::default());
        let one = asking(&ask_box, "read ~/w/one.txt", Duration::from_secs(30));
        let first = first_when(&ask_box, |_| true)?;
        dialog.sync(Some(&first), Instant::now());
        let two = asking(&ask_box, "read ~/w/two.txt", Duration::from_secs(30));
        let waiting = first_when(&ask_box, |asked| asked.behind == 1)?;
        assert_eq!(waiting.id, first.id, "the newer question waits behind");

        // The newer question replaces nothing: the first dialog stays up.
        dialog.sync(Some(&waiting), Instant::now());
        assert_eq!(calls(&dialog), [format!("show {}", first.id)]);

        // A click on the first dialog's Allow (its second button) answers the
        // first question; the next sync closes it and shows the second.
        let allow = button_answer(1).ok_or("a second button")?;
        assert!(ask_box.answer(first.id, allow));
        assert_eq!(one.join().ok(), Some(AskAnswer::Allow));
        let second = ask_box.head().ok_or("the second question")?;
        assert_eq!(second.what, "read ~/w/two.txt");
        assert_eq!(
            dialog.sync(Some(&second), Instant::now()),
            Some(Closed::Gone(first.id))
        );
        assert_eq!(dialog.shown(), Some(second.id));

        // A late click on the first dialog answers nothing, not the second.
        assert!(!ask_box.answer(first.id, AskAnswer::Allow));
        assert_eq!(ask_box.head().map(|asked| asked.id), Some(second.id));

        // Return in the second dialog: its first button, Deny.
        let deny = button_answer(0).ok_or("a first button")?;
        assert!(ask_box.answer(second.id, deny));
        assert_eq!(two.join().ok(), Some(AskAnswer::Deny));
        assert_eq!(
            dialog.sync(ask_box.head().as_ref(), Instant::now()),
            Some(Closed::Gone(second.id))
        );
        assert_eq!(
            calls(&dialog),
            [
                format!("show {}", first.id),
                format!("close {}", first.id),
                format!("show {}", second.id),
                format!("close {}", second.id),
            ]
        );
        Ok(())
    }

    /// The question shown runs out of time in the tick the next one shows,
    /// and an answer for it comes in that tick: a click on its dialog, or on
    /// a menu item shown before. The answer reaches that question or nothing,
    /// never the next one, whose Allow is not even armed yet.
    #[test]
    fn an_answer_for_a_question_out_of_time_never_answers_the_next() -> Result<()> {
        // The dialog's clock first: the engine's has not taken the question
        // out of the line yet, so the answer is still that question's.
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let mut dialog = AskDialog::new(Recorder::default());
        let one = asking(&ask_box, "read ~/w/one.txt", Duration::from_secs(30));
        let first = first_when(&ask_box, |_| true)?;
        let two = asking(&ask_box, "read ~/w/two.txt", Duration::from_secs(30));
        first_when(&ask_box, |asked| asked.behind == 1)?;
        dialog.sync(
            ask_box.head().as_ref(),
            asked_at(&first, Duration::from_secs(30))?,
        );
        assert_eq!(
            dialog.sync(ask_box.head().as_ref(), first.deadline),
            Some(Closed::Expired(first.id))
        );
        assert!(ask_box.answer(first.id, AskAnswer::Allow), "applied to it");
        assert_eq!(one.join().ok(), Some(AskAnswer::Allow));
        let second = ask_box.head().ok_or("the second question")?;
        assert_ne!(second.id, first.id);
        assert_eq!(dialog.sync(Some(&second), first.deadline), None);
        assert_eq!(dialog.shown(), Some(second.id));
        assert!(
            !ask_box.answer(first.id, AskAnswer::Allow),
            "a second click for it answers nothing"
        );
        assert_eq!(ask_box.head().map(|asked| asked.id), Some(second.id));
        assert!(!two.is_finished(), "the second still waits");
        assert!(ask_box.answer(second.id, AskAnswer::Deny));
        assert_eq!(two.join().ok(), Some(AskAnswer::Deny), "its own answer");

        // The engine's clock first: the question was refused as expired, the
        // next one shows in that tick, and the answer that comes then answers
        // nothing.
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let mut dialog = AskDialog::new(Recorder::default());
        let one = asking(&ask_box, "read ~/w/one.txt", Duration::from_secs(2));
        let first = first_when(&ask_box, |_| true)?;
        dialog.sync(Some(&first), asked_at(&first, Duration::from_secs(2))?);
        let two = asking(&ask_box, "read ~/w/two.txt", Duration::from_secs(30));
        assert_eq!(one.join().ok(), Some(AskAnswer::Expired));
        let second = first_when(&ask_box, |asked| asked.id != first.id)?;
        assert_eq!(
            dialog.sync(Some(&second), first.deadline),
            Some(Closed::Gone(first.id)),
            "the first dialog goes and the second shows, in one tick"
        );
        assert_eq!(dialog.shown(), Some(second.id));
        assert!(
            !ask_box.answer(first.id, AskAnswer::Allow),
            "refused: it answers nothing"
        );
        assert_eq!(ask_box.head().map(|asked| asked.id), Some(second.id));
        assert!(!two.is_finished(), "the second still waits");
        assert_eq!(
            dialog.next_wake(),
            first.deadline.checked_add(ARM_DELAY),
            "its Allow arms a full ARM_DELAY after it showed"
        );
        assert_eq!(
            calls(&dialog),
            [
                format!("show {}", first.id),
                format!("close {}", first.id),
                format!("show {}", second.id),
            ],
            "nothing armed: no click in that tick can allow"
        );
        assert!(ask_box.answer(second.id, AskAnswer::Deny));
        assert_eq!(two.join().ok(), Some(AskAnswer::Deny), "its own answer");
        Ok(())
    }

    /// A question that waited past its deadline behind another is never
    /// shown; one that waited but has time left shows with the time it has.
    #[test]
    fn a_question_out_of_time_is_not_shown() {
        let now = Instant::now();
        let late = Asked {
            id: 4,
            what: "read x".to_owned(),
            at: "2026-01-15T09:30:00Z".to_owned(),
            deadline: now,
            behind: 0,
        };
        let mut dialog = AskDialog::new(Recorder::default());
        assert_eq!(dialog.sync(Some(&late), now), None);
        assert!(dialog.presenter().calls.is_empty());

        let soon = Asked {
            id: 5,
            deadline: now + Duration::from_millis(4_200),
            ..late
        };
        dialog.sync(Some(&soon), now);
        assert_eq!(
            dialog.presenter().calls,
            [
                "show 5: The agent asks | read x\n\nAsked at 2026-01-15T09:30:00Z. \
              With no answer within 5 s, it is refused."
            ]
        );
    }

    #[test]
    fn the_buttons_and_the_menu_items_name_their_answer() {
        assert_eq!(BUTTONS.map(|(title, _)| title), ["Deny", "Allow"]);
        assert_eq!(
            button_answer(0),
            Some(AskAnswer::Deny),
            "the default, Return"
        );
        assert_eq!(button_answer(1), Some(AskAnswer::Allow));
        for closed_by_tulpa in [2, -1, -2001, isize::MIN] {
            assert_eq!(button_answer(closed_by_tulpa), None, "{closed_by_tulpa}");
        }

        assert_eq!(allow_item(7), "tulpa.allow.7");
        assert_eq!(menu_answer(&allow_item(7)), Some((7, AskAnswer::Allow)));
        assert_eq!(
            menu_answer(&deny_item(u64::MAX)),
            Some((u64::MAX, AskAnswer::Deny))
        );
        for not_one in [
            "tulpa.allow.",
            "tulpa.allow.+7",
            "tulpa.allow. 7",
            "tulpa.allow.7x",
            "tulpa.allow.18446744073709551616",
            "tulpa.allow",
            "tulpa.pause",
        ] {
            assert_eq!(menu_answer(not_one), None, "{not_one}");
        }
    }

    #[test]
    fn the_notification_points_at_the_dialog() {
        let asked = Asked {
            id: 0,
            what: "take one photo with the camera".to_owned(),
            at: "2026-01-15T09:30:00Z".to_owned(),
            deadline: Instant::now(),
            behind: 0,
        };
        assert_eq!(
            notice(&asked),
            "take one photo with the camera — answer in the Tulpa dialog"
        );
    }
}
