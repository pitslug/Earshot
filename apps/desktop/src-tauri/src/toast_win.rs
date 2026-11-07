//! Windows notifications (T7.5): WinRT's `ToastNotificationManager`, no `unsafe`.
//!
//! An app without a package needs its app id registered per user before Windows names it on a
//! notification (`HKCU\Software\Classes\AppUserModelId\<id>`, written with `reg.exe` through
//! `earshot_proc` the first time one is shown; nothing else is stored). Clicks arrive through
//! the notification's `Activated` event while the process runs; Earshot stays in the tray, so
//! it does. The last notification is kept alive here, or its handler would go with it. One that
//! stays on screen is kept apart, so it can be taken back after another has been shown.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
};
use windows::core::{HSTRING, IInspectable, Interface};

use crate::shell::{Toast, ToastClick};

/// The app id (`identifier` in `tauri.conf.json`).
pub const APP_ID: &str = "earshot";

/// What a click does.
pub type OnClick = Arc<dyn Fn(ToastClick) + Send + Sync>;

/// `text` for an XML attribute or element.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The notification's XML: silent, its click and buttons as activation arguments.
#[must_use]
pub fn toast_xml(toast: &Toast) -> String {
    let actions: String = toast
        .buttons
        .iter()
        .map(|(label, click)| {
            format!(
                "<action content=\"{}\" arguments=\"{}\"/>",
                escaped(label),
                click.argument()
            )
        })
        .collect();
    let actions = if actions.is_empty() {
        String::new()
    } else {
        format!("<actions>{actions}</actions>")
    };
    let scenario = if toast.stays {
        " scenario=\"reminder\""
    } else {
        ""
    };
    format!(
        "<toast launch=\"{}\"{scenario}><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><audio silent=\"true\"/>{actions}</toast>",
        toast.click.argument(),
        escaped(&toast.title),
        escaped(&toast.body),
    )
}

/// Shows notifications and keeps the last one alive.
#[derive(Default)]
pub struct Toaster {
    registered: OnceLock<()>,
    last: Mutex<Option<Shown>>,
    staying: Mutex<Option<Shown>>,
}

/// A notification and what showed it.
type Shown = (ToastNotification, windows::UI::Notifications::ToastNotifier);

impl Toaster {
    /// Shows `toast`.
    ///
    /// # Errors
    /// Windows' error, when it refuses the notification.
    pub fn show(&self, toast: &Toast, on_click: OnClick) -> windows::core::Result<()> {
        self.registered.get_or_init(register_app_id);
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(toast_xml(toast)))?;
        let notification = ToastNotification::CreateToastNotification(&xml)?;
        notification.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_, args| {
                let argument = args
                    .as_ref()
                    .and_then(|a| a.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|a| a.Arguments().ok())
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                on_click(ToastClick::from_argument(&argument));
                Ok(())
            },
        ))?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?;
        if toast.stays {
            self.hide_staying();
        }
        notifier.Show(&notification)?;
        let slot = if toast.stays {
            &self.staying
        } else {
            &self.last
        };
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((notification, notifier));
        Ok(())
    }

    /// Takes the notification that stays off the screen, if there is one. One already gone is no
    /// error.
    pub fn hide_staying(&self) {
        let shown = self
            .staying
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((notification, notifier)) = shown {
            let _ = notifier.Hide(&notification);
        }
    }
}

/// Names the app id "Earshot" for this user. A failure leaves the notification with the bare id.
fn register_app_id() {
    let key = format!(r"HKCU\Software\Classes\AppUserModelId\{APP_ID}");
    let _ = crate::general::run_reg(
        &[
            "add",
            &key,
            "/v",
            "DisplayName",
            "/t",
            "REG_SZ",
            "/d",
            "Earshot",
            "/f",
        ],
        crate::general::CHANGE_FAILED,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_app_id_is_the_identifier_in_the_tauri_config() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], APP_ID);
    }

    #[test]
    fn shell_toast_xml_is_silent_and_carries_its_clicks() {
        let xml = toast_xml(&crate::shell::summary_toast(1, 2, 1));
        assert!(xml.contains("<audio silent=\"true\"/>"), "{xml}");
        assert!(xml.starts_with("<toast launch=\"meeting\">"), "{xml}");
        assert!(xml.contains("<text>Summary ready</text>"), "{xml}");
        assert!(
            xml.contains("content=\"Name voices\" arguments=\"names\""),
            "{xml}"
        );
        let parsed = XmlDocument::new().unwrap();
        parsed.LoadXml(&HSTRING::from(xml)).unwrap();
    }

    #[test]
    fn shell_toast_xml_without_buttons_has_no_actions_and_escapes_text() {
        let xml = toast_xml(&Toast {
            title: "a & b".into(),
            body: "<c> \"d\"".into(),
            click: ToastClick::Window,
            buttons: Vec::new(),
            stays: false,
        });
        assert!(!xml.contains("actions"), "{xml}");
        assert!(!xml.contains("scenario"), "{xml}");
        assert!(xml.contains("a &amp; b"), "{xml}");
        let parsed = XmlDocument::new().unwrap();
        parsed.LoadXml(&HSTRING::from(xml)).unwrap();
    }

    #[test]
    fn shell_toast_xml_of_the_call_end_question_stays_silently_with_both_answers() {
        let xml = toast_xml(&crate::shell::call_end_toast());
        assert!(
            xml.starts_with("<toast launch=\"window\" scenario=\"reminder\">"),
            "{xml}"
        );
        assert!(xml.contains("<audio silent=\"true\"/>"), "{xml}");
        assert!(
            xml.contains("content=\"Stop recording\" arguments=\"stop\""),
            "{xml}"
        );
        assert!(
            xml.contains("content=\"Keep recording\" arguments=\"keep\""),
            "{xml}"
        );
        let parsed = XmlDocument::new().unwrap();
        parsed.LoadXml(&HSTRING::from(xml)).unwrap();
    }
}
