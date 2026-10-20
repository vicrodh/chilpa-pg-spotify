use crate::{auth::Client, snapshot::Store, state::now};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const KEY: &str = "library-enrichment:v1";
const FEATURES: [&str; 4] = [
    "artist_genres",
    "artist_genres_single",
    "album_label",
    "track_credits",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
// Retain the previous diagnostic schema when merging entries.
pub(crate) enum GraphqlCode {
    #[serde(rename = "PersistedQueryNotFound")]
    PersistedQueryNotFound,
    #[serde(rename = "PersistedQueryNotSupported")]
    PersistedQueryNotSupported,
    Unauthenticated,
    Unauthorized,
    Forbidden,
    NotFound,
    BadUserInput,
    InternalServerError,
    RateLimited,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimeoutStage {
    Identity,
    Gate,
    Token,
    Request,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Ok,
    CacheHit,
    NoCandidates,
    NotAttempted,
    NoSession,
    NoToken,
    Disabled {
        #[serde(default)]
        remaining: u64,
    },
    UnexpectedShape,
    Transport,
    Http {
        status: u16,
    },
    Graphql {
        code: GraphqlCode,
    },
    Timeout {
        stage: TimeoutStage,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Attempt {
    pub at: u64,
    pub outcome: Outcome,
    pub asked: usize,
    pub with_data: usize,
    pub cached: usize,
}

impl Attempt {
    pub fn start() -> Self {
        Self {
            at: now(),
            outcome: Outcome::Ok,
            asked: 0,
            with_data: 0,
            cached: 0,
        }
    }

    pub fn fail(&mut self, outcome: Outcome) {
        if matches!(
            self.outcome,
            Outcome::Ok | Outcome::CacheHit | Outcome::NoCandidates
        ) {
            self.outcome = outcome;
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Pause {
    until: u64,
    status: u16,
}

impl Client {
    pub(crate) fn enrichment_pause(&self, identity: &str, route: &str) -> Option<Outcome> {
        let key = format!("capability:enrichment:{route}");
        let saved = self
            .snapshots
            .peek(identity, &key)
            .and_then(|v| serde_json::from_value::<Pause>(v).ok());
        let mut pauses = self.enrichment_pauses.lock().unwrap();
        let entry = pauses
            .entry((identity.into(), route.into()))
            .or_insert(Pause {
                until: 0,
                status: 403,
            });
        if let Some(saved) = saved.filter(|saved| saved.until > entry.until) {
            *entry = saved;
        }
        let remaining = entry.until.saturating_sub(now());
        (remaining > 0).then_some(if entry.status == 403 {
            Outcome::Disabled { remaining }
        } else {
            Outcome::Http {
                status: entry.status,
            }
        })
    }

    pub(crate) fn pause_enrichment(&self, identity: &str, route: &str, outcome: Outcome) {
        let seconds = match outcome {
            Outcome::Http { status: 403 } => 3600,
            Outcome::Http { status: 429 } => 60,
            _ => return,
        };
        let Outcome::Http { status } = outcome else {
            return;
        };
        let pause = Pause {
            until: now() + seconds,
            status,
        };
        self.enrichment_pauses
            .lock()
            .unwrap()
            .insert((identity.into(), route.into()), pause);
        let _ = self.snapshots.put(
            identity,
            &format!("capability:enrichment:{route}"),
            json!(pause),
        );
    }
}

pub(crate) fn record(store: &Store, identity: &str, feature: &str, attempt: Attempt) -> Result<()> {
    if !FEATURES.contains(&feature) {
        bail!("Unknown library enrichment.");
    }
    store.update(identity, KEY, |previous| {
        let previous = previous.unwrap_or_default();
        let mut entries = serde_json::Map::new();
        for name in FEATURES {
            if let Ok(saved) = serde_json::from_value::<Attempt>(previous[name].clone()) {
                entries.insert(name.into(), json!(saved));
            }
        }
        entries.insert(feature.into(), json!(attempt));
        Value::Object(entries)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn attempts_preserve_counts_and_the_first_failure() {
        let mut attempt = Attempt::start();
        assert!(attempt.at <= now());
        attempt.asked = 20;
        attempt.with_data = 3;
        attempt.cached = 7;
        attempt.fail(Outcome::Http { status: 403 });
        attempt.fail(Outcome::UnexpectedShape);
        let value = json!(attempt);
        assert_eq!(value.as_object().unwrap().len(), 5);
        assert_eq!(value["asked"], 20);
        assert_eq!(value["with_data"], 3);
        assert_eq!(value["cached"], 7);
        assert_eq!(value["outcome"], json!({"class":"http","status":403}));
        for outcome in [
            Outcome::NoSession,
            Outcome::NoToken,
            Outcome::Timeout {
                stage: TimeoutStage::Identity,
            },
            Outcome::Timeout {
                stage: TimeoutStage::Gate,
            },
            Outcome::Timeout {
                stage: TimeoutStage::Token,
            },
            Outcome::Timeout {
                stage: TimeoutStage::Request,
            },
        ] {
            assert_eq!(
                serde_json::from_value::<Outcome>(json!(outcome)).unwrap(),
                outcome
            );
        }
    }

    #[test]
    fn records_merge_features_and_isolate_accounts_without_extra_fields() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().to_owned()).unwrap();
        let mut genres = Attempt::start();
        genres.asked = 50;
        genres.with_data = 0;
        genres.outcome = Outcome::Http { status: 403 };
        let mut previous = json!({"private-token":"private-name","artist_genres":genres});
        previous["artist_genres"]["body"] = json!("private-name");
        store.put("first", KEY, previous).unwrap();
        let mut label = Attempt::start();
        label.asked = 2;
        label.with_data = 1;
        record(&store, "first", "album_label", label.clone()).unwrap();
        record(&store, "second", "track_credits", Attempt::start()).unwrap();
        let saved = store.peek("first", KEY).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 2);
        assert_eq!(saved["artist_genres"], json!(genres));
        assert_eq!(saved["album_label"], json!(label));
        assert!(!saved.to_string().contains("private-"));
        assert!(
            store
                .peek("second", KEY)
                .unwrap()
                .get("album_label")
                .is_none()
        );
        assert!(record(&store, "first", "private-name", Attempt::start()).is_err());
        let restarted = Store::new(directory.path().to_owned()).unwrap();
        assert_eq!(restarted.peek("first", KEY).unwrap(), saved);
    }

    #[test]
    fn simultaneous_features_do_not_drop_each_others_updates() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(directory.path().to_owned()).unwrap());
        let ready = Arc::new(Barrier::new(FEATURES.len()));
        let threads: Vec<_> = FEATURES
            .into_iter()
            .map(|feature| {
                let store = store.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    ready.wait();
                    for count in 1..=8 {
                        let mut attempt = Attempt::start();
                        attempt.asked = count;
                        record(&store, "first", feature, attempt).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let saved = store.peek("first", KEY).unwrap();
        for feature in FEATURES {
            assert_eq!(saved[feature]["asked"], 8);
        }
    }
}
