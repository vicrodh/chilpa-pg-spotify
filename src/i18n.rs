use serde_json::Value;
use std::{collections::HashMap, sync::OnceLock};

fn catalog() -> &'static HashMap<String, String> {
    static SPANISH: OnceLock<HashMap<String, String>> = OnceLock::new();
    SPANISH.get_or_init(|| {
        serde_json::from_str(include_str!("../locales/es.json"))
            .expect("valid embedded translations")
    })
}

pub fn text(english: &str, spanish: bool) -> String {
    if !spanish {
        return english.into();
    }
    if let Some(translated) = catalog().get(english) {
        return translated.clone();
    }
    // Each owned message template has at most one interpolation. I preserve
    // that value verbatim, including account names and server wait intervals.
    for (template, translated) in catalog() {
        let Some(start) = template.find('{') else {
            continue;
        };
        let Some(end) = template[start..].find('}').map(|i| i + start + 1) else {
            continue;
        };
        let (prefix, suffix) = (&template[..start], &template[end..]);
        let Some(value) = english
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        return translated.replace(&template[start..end], value);
    }
    english.into()
}

fn field(value: &mut Value, key: &str) {
    if let Some(english) = value[key].as_str() {
        value[key] = Value::String(text(english, true));
    }
}
fn labels(value: &mut Value, key: &str) {
    if let Some(values) = value.as_array_mut() {
        for value in values {
            field(value, key);
        }
    }
}

pub fn document(method: &str, params: &Value, value: &mut Value, spanish: bool) {
    if !spanish {
        return;
    }
    if method.starts_with("settings.") || method == "auth.login" {
        field(&mut value["status"], "label");
        labels(&mut value["content"], "text");
        labels(&mut value["links"], "label");
        labels(&mut value["form"]["fields"], "label");
        labels(&mut value["form"]["actions"], "label");
        if let Some(notices) = value["notices"].as_array_mut() {
            for notice in notices {
                if let Some(s) = notice.as_str() {
                    *notice = Value::String(text(s, true));
                }
            }
        }
    } else {
        labels(&mut value["tabs"], "title");
        if params["ref"]
            .as_str()
            .is_some_and(|s| s.starts_with("radio:"))
        {
            field(value, "title");
            field(&mut value["header"], "title");
            field(&mut value["playlist"], "title");
        }
        // Provider titles, artist names, account names, lyrics, references and
        // URLs never pass through this catalog.
        if method == "home"
            || (method == "page.get"
                && params["ref"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("discover")))
        {
            field(value, "title");
            labels(&mut value["tabs"], "title");
        }
        if let Some(shelves) = value.get_mut("shelves") {
            labels(shelves, "title");
        }
        field(value, "cache_notice");
        field(value, "warning");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn english_is_default_and_spanish_preserves_provider_text_and_interpolations() {
        assert_eq!(text("For you", false), "For you");
        assert_eq!(text("For you", true), "Para ti");
        assert!(text("Library and playback authorized as Some Name. You can now browse Discover and play music.",true).contains("Some Name"));
        let mut page = serde_json::json!({"title":"For you","shelves":[{"title":"Made for you","items":[{"title":"For you","ref":"playlist:123"}]}]});
        document("home", &Value::Null, &mut page, true);
        assert_eq!(page["shelves"][0]["title"], "Hecho para ti");
        assert_eq!(page["shelves"][0]["items"][0]["title"], "For you");
        assert_eq!(page["shelves"][0]["items"][0]["ref"], "playlist:123");
    }
}
