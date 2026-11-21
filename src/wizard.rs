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
            json!({"text":"Register Chilpa with Spotify once, on an account with Premium. It takes about two minutes and gives you a Client ID. Your own Client ID keeps your requests out of the shared quota."}),
            json!({"text":"1. Open Spotify for Developers (link below) and sign in."}),
            json!({"text":"2. Select Create app. Any name will do."}),
            json!({"text":"3. Under Redirect URIs, add exactly the callback URL shown below."}),
            json!({"text":"4. Tick Web API, accept the terms and save. Then open Users Management and add your Spotify account."}),
            json!({"text":"5. Copy the Client ID from the app's page, paste it below and continue. No Client Secret is needed."}),
        ]);
        fields.push(text_field(
            "callback_url",
            "Callback URL to add under Redirect URIs (select and copy)",
            false,
            256,
        ));
        fields.push(text_field(
            "client_id",
            "Client ID (32 letters and numbers)",
            false,
            32,
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
        content.push(json!({"text":if view.client_id.is_empty() {"You are using shared access. Your own Client ID would keep your requests out of the shared quota."} else {"You are using your own Client ID."}}));
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
    let mut wizard = json!({"step":step,"total":3,"id":id});
    // The stepper tracks the three-part setup. Shared search on an account that
    // is already connected is a side trip, so it keeps the plain page.
    let side_trip = view.pending.as_ref().is_some_and(|flow| flow.shared);
    if !side_trip {
        wizard["title"] = json!(match id {
            "client" => "Register Chilpa with Spotify",
            "library_browser" => "Connect your library",
            "playback_browser" | "playback" => "Authorize playback",
            _ => "Spotify is connected",
        });
        let states = (1..=3).map(|n| {
            if id == "done" || n < step { "complete" } else if n == step { "current" } else { "upcoming" }
        });
        wizard["steps"] = json!(["Client ID", "Library", "Playback"]
            .iter()
            .zip(states)
            .map(|(label, state)| json!({"label":label,"state":state}))
            .collect::<Vec<_>>());
    }
    json!({"revision":view.revision,"wizard":wizard,
        "progress":{"done":if id=="done" {3} else {step-1},"total":3},
        "status":{"phase":if waiting {"pending"} else {"ready"},"label":label},
        "form":{"fields":fields,"actions":actions,"unsupported":[]},"values":values,
        "content":content,"links":links,"notices":notices})
}

fn text_field(key: &str, label: &str, required: bool, max_length: usize) -> Value {
    json!({"key":key,"label":label,"required":required,"control":{"type":"text","max_length":max_length}})
}
fn action(key: &str, label: &str, fields: &[&str]) -> Value {
    let mut action = json!({"key":key,"label":label,"fields":fields});
    if key == "sign_out" { action["destructive"] = json!("account"); }
    action
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
    fn the_stepper_marks_what_is_done_what_is_current_and_what_is_next() {
        let states = |page: &Value| -> Vec<String> {
            page["wizard"]["steps"].as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap().to_owned()).collect()
        };
        let setup = render(view());
        assert_eq!(states(&setup), ["current", "upcoming", "upcoming"]);
        assert_eq!(setup["wizard"]["title"], "Register Chilpa with Spotify");
        let mut connected = view();
        connected.signed_in = true;
        assert_eq!(states(&render(connected)), ["complete", "complete", "current"]);
        let mut ready = view();
        ready.signed_in = true;
        ready.playback_ready = true;
        assert_eq!(states(&render(ready)), ["complete", "complete", "complete"]);
    }

    #[test]
    fn the_first_step_numbers_what_to_do_in_the_dashboard() {
        let setup = render(view());
        let lines: Vec<&str> = setup["content"].as_array().unwrap().iter().map(|c| c["text"].as_str().unwrap()).collect();
        for number in 1..=5 {
            assert!(lines.iter().any(|line| line.starts_with(&format!("{number}. "))), "step {number} is missing");
        }
        assert_eq!(setup["form"]["fields"][0]["key"], "callback_url");
        assert_eq!(setup["form"]["fields"][1]["key"], "client_id");
        assert_eq!(setup["form"]["actions"][0]["key"], "use_personal");
    }

    #[test]
    fn shared_search_on_a_connected_account_keeps_the_plain_page() {
        let mut current = view();
        current.signed_in = true;
        current.pending = Some(Pending {
            playback: false,
            shared: true,
            redirect: "http://127.0.0.1:8989/login",
            authorization: "https://accounts.spotify.com/authorize",
            returned_address: "",
            error: None,
            expired: false,
        });
        let page = render(current);
        assert!(page["wizard"].get("steps").is_none());
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
