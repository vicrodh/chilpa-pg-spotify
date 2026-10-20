use crate::{
    auth::Client,
    enrichment::{GraphqlCode, Outcome, TimeoutStage, graphql_error},
};
use serde_json::Value;
use tokio::time::{Instant, timeout_at};

impl Client {
    pub(crate) async fn enrichment_tokens(
        &self,
        deadline: Instant,
    ) -> Result<(String, String), Outcome> {
        let tokens = async {
            #[cfg(test)]
            if self.partner_fixture.is_some() {
                return self.partner_tokens().await.map_err(|_| Outcome::NoToken);
            }
            if !self.has_session().await {
                return Err(Outcome::NoSession);
            }
            self.partner_tokens().await.map_err(|_| Outcome::NoToken)
        };
        timeout_at(deadline, tokens)
            .await
            .unwrap_or(Err(Outcome::Timeout {
                stage: TimeoutStage::Token,
            }))
    }

    pub(crate) async fn enrichment_query(
        &self,
        query: &Value,
        tokens: &(String, String),
        route: &str,
    ) -> Result<Value, Outcome> {
        let mut request = self.partner_request(tokens);
        if query["operationName"] == "getAlbum" {
            request = request.header("spotify-app-version", "896000000");
        }
        let response = request
            .json(query)
            .send()
            .await
            .map_err(|_| Outcome::Transport)?;
        let status = response.status().as_u16();
        if status == 429 {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            self.partner_limits
                .lock()
                .await
                .record(route, seconds, false);
            return Err(Outcome::Http { status });
        }
        let value = Self::response_json(response).await.map_err(|_| {
            if (200..300).contains(&status) {
                Outcome::UnexpectedShape
            } else {
                Outcome::Http { status }
            }
        })?;
        if let Some(outcome) = graphql_error(&value) {
            if outcome
                == (Outcome::Graphql {
                    code: GraphqlCode::RateLimited,
                })
            {
                self.partner_limits.lock().await.record(route, 60, false);
            }
            return Err(outcome);
        }
        if !(200..300).contains(&status) {
            return Err(Outcome::Http { status });
        }
        Ok(value)
    }
}
