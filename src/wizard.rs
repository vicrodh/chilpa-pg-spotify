use serde_json::{Value, json};

pub struct View<'a> {
    pub revision: u64,
    pub account_name: &'a str,
    pub client_id: &'a str,
    pub signed_in: bool,
    pub playback_ready: bool,
    pub shared_ready: bool,
    pub playlist_writing: bool,
    pub configure: bool,
    pub advanced: bool,
    pub pending: Option<Pending<'a>>,
    pub notice: Option<&'a str>,
    pub library_reauth: bool,
}

pub struct Pending<'a> {
    pub playback: bool,
    pub shared: bool,
    pub redirect: &'a str,
    pub authorization: &'a str,
    pub returned_address: &'a str,
    pub error: Option<&'a str>,
    pub expired: bool,
}

pub fn render(view: View<'_>) -> Value {
    let mut fields = vec![];
    let mut actions = vec![];
    let mut links = vec![];
    let mut values = json!({});
    let mut notices: Vec<String> = view.notice.map(str::to_owned).into_iter().collect();
    let mut content = vec![];
    if view.library_reauth {
        notices.push("Library access needs to be reconnected. Return to Client ID and authorize the same account to keep playback access.".into());
    }
    let (step, id, label, waiting);
    if let Some(ref flow) = view.pending {
        step = if flow.playback { 3 } else { 2 };
        id = if flow.playback {
            "playback_browser"
        } else {
            "library_browser"
        };
        label = if flow.shared {
            "Enable shared search: open Spotify and approve access"
        } else if flow.playback {
            "Step 3 of 3: open Spotify and authorize playback"
        } else {
            "Step 2 of 3: open Spotify and connect your library"
        };
        waiting = flow.error.is_none() && !flow.expired && flow.returned_address.is_empty();
        content.push(json!({"text":if flow.shared { "Approve shared search with the same account as your library. Your primary connection and playback are preserved." } else if flow.playback {
            "Your library is connected. Click Open Spotify below and approve playback with the same account. Spotify requires this separate approval."
        } else {
            "Click Open Spotify below and approve library access. Return to Chilpa; the wizard will advance automatically."
        }}));
        fields.push(text_field(
            "callback_url",
            "Callback URL (generated automatically)",
            false,
            256,
        ));
        values["callback_url"] = json!(flow.redirect);
        if let Some(error) = flow.error {
            notices.push(error.into());
        }
        if flow.expired {
            notices.push(
                "This step expired. Click Retry this step to start a new authorization.".into(),
            );
        }
        if flow.error.is_some() || flow.expired {
            actions.push(action("retry", "Retry this step", &[]));
        } else {
            links.push(json!({"label":if flow.shared {"Open Spotify: enable shared search"} else if flow.playback {"Open Spotify: authorize playback"} else {"Open Spotify: connect library"},"url":flow.authorization}));
        }
        if view.advanced {
            fields.push(
                json!({"key":"redirect_url","label":"Browser response (manual fallback)",
                "required":false,"control":{"type":"secret","max_length":8192}}),
            );
            values["redirect_url"] = json!(flow.returned_address);
            actions.push(action(
                "finish",
                "Complete with manual response",
                &["redirect_url"],
            ));
        }
        actions.push(action("cancel", "Back to previous step", &[]));
        actions.push(action(
            "advanced",
            if view.advanced {
                "Hide manual fallback"
            } else {
                "Show manual fallback"
            },
            &[],
        ));
    } else if view.configure || !view.signed_in {
        step = 1;
        id = "client";
        label = "Step 1 of 3: choose Spotify access";
        waiting = false;
        content.extend([
            json!({"text":"I recommend your own Client ID for better results: your app requests will not share the public app quota. Spotify also limits personal developer apps."}),
            json!({"text":"Paste your app's Client ID. To create one, open Spotify for Developers, register the callback URL below and add your account in Users Management. No Client Secret is needed."}),
        ]);
        fields.push(text_field(
            "client_id",
            "Your Client ID (recommended)",
            false,
            32,
        ));
        fields.push(text_field(
            "callback_url",
            "Callback URL to register in your app",
            false,
            256,
        ));
        values = json!({"client_id":view.client_id,"callback_url":"http://127.0.0.1:8989/login"});
        links.push(json!({"label":"Open Spotify for Developers","url":"https://developer.spotify.com/dashboard"}));
        actions.push(action(
            "use_personal",
            "Continue with my Client ID (recommended)",
            &["client_id"],
        ));
        actions.push(action("use_shared", "Continue with shared access", &[]));
        if view.signed_in {
            actions.push(action("cancel", "Keep current connection", &[]));
        }
    } else if !view.playback_ready {
        step = 3;
        id = "playback";
        label = "Step 3 of 3: enable playback";
        waiting = false;
        content.push(json!({"text":format!("Library connected as {}. Authorize playback on this device: click Continue, then Open Spotify on the next screen.",view.account_name)}));
        actions.push(action("playback", "Continue: authorize playback", &[]));
        actions.push(action("configure", "Back to Client ID", &[]));
        actions.push(action("sign_out", "Sign out", &[]));
    } else {
        step = 3;
        id = "done";
        label = "Ready: Spotify is connected";
        waiting = false;
        content.push(json!({"text":format!("Library and playback authorized as {}. You can now browse Discover and play music.",view.account_name)}));
        content.push(json!({"text":if view.client_id.is_empty() {"You are using shared access. I recommend your own Client ID to separate its request quota."} else {"You are using your own Client ID."}}));
        actions.push(action("configure", "Change Client ID or account", &[]));
        actions.push(action("check_library", "Check library access", &[]));
        actions.push(action("sign_out", "Sign out", &[]));
    }
    if view.signed_in && view.pending.is_none() && !view.configure {
        if !view.shared_ready {
            actions.push(action("enable_shared", "Enable shared search", &[]));
        }
        if !view.playlist_writing {
            actions.push(action(
                "enable_playlist_writing",
                "Enable playlist writing",
                &[],
            ));
        }
    }
    json!({"revision":view.revision,"wizard":{"step":step,"total":3,"id":id},
        "progress":{"done":if id=="done" {3} else {step-1},"total":3},
        "status":{"phase":if waiting {"pending"} else {"ready"},"label":label},
        "form":{"fields":fields,"actions":actions,"unsupported":[]},"values":values,
        "content":content,"links":links,"notices":notices})
}

fn text_field(key: &str, label: &str, required: bool, max_length: usize) -> Value {
    json!({"key":key,"label":label,"required":required,"control":{"type":"text","max_length":max_length}})
}
fn action(key: &str, label: &str, fields: &[&str]) -> Value {
    json!({"key":key,"label":label,"fields":fields})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> View<'static> {
        View {
            revision: 1,
            account_name: "Test",
            client_id: "",
            signed_in: false,
            playback_ready: false,
            shared_ready: false,
            playlist_writing: false,
            configure: false,
            advanced: false,
            pending: None,
            notice: None,
            library_reauth: false,
        }
    }
    #[test]
    fn the_wizard_starts_with_personal_access_and_advances_to_playback() {
        let setup = render(view());
        assert_eq!(setup["wizard"]["id"], "client");
        let mut connected = view();
        connected.signed_in = true;
        let playback = render(connected);
        assert_eq!(playback["wizard"]["id"], "playback");
        assert_eq!(playback["progress"]["done"], 2);
        let mut ready = view();
        ready.signed_in = true;
        ready.playback_ready = true;
        let complete = render(ready);
        assert_eq!(complete["wizard"]["id"], "done");
        assert_eq!(complete["progress"]["done"], 3);
        assert_eq!(complete["status"]["phase"], "ready");
    }
    #[test]
    fn browser_steps_say_what_to_click_and_failed_steps_stop_polling() {
        for playback in [false, true] {
            let mut current = view();
            current.pending = Some(Pending {
                playback,
                shared: false,
                redirect: "http://127.0.0.1:8898/login",
                authorization: "https://accounts.spotify.com/authorize",
                returned_address: "",
                error: None,
                expired: false,
            });
            let browser = render(current);
            assert_eq!(browser["status"]["phase"], "pending");
            assert!(
                browser["status"]["label"]
                    .as_str()
                    .unwrap()
                    .contains("open Spotify")
            );
            assert!(
                !browser["status"]["label"]
                    .as_str()
                    .unwrap()
                    .contains("Waiting")
            );
            let mut current = view();
            current.pending = Some(Pending {
                playback,
                shared: false,
                redirect: "http://127.0.0.1:8898/login",
                authorization: "https://accounts.spotify.com/authorize",
                returned_address: "",
                error: Some("Rejected"),
                expired: false,
            });
            let failed = render(current);
            assert_eq!(failed["status"]["phase"], "ready");
            assert!(failed["links"].as_array().unwrap().is_empty());
            assert!(
                failed["form"]["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["key"] == "retry")
            );
        }
    }
}
