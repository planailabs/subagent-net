//! HTTP client for the RPC endpoints. Takes several base URLs (hub HA) and
//! moves on when one is unreachable or answers "not the leader".

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use crate::{ErrorKind, Op, OpError};

/// Header a standby hub uses to point at the leader.
pub const LEADER_HEADER: &str = "x-subnet-leader";

pub struct Client {
    bases: Vec<String>,
    current: AtomicUsize,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    /// `bases` is a comma-separated list of hub URLs.
    pub fn new(bases: &str, token: Option<String>) -> Self {
        let bases = bases.split(',').map(|b| b.trim().trim_end_matches('/').to_string()).filter(|b| !b.is_empty()).collect();
        Self { bases, current: AtomicUsize::new(0), token, http: reqwest::Client::new() }
    }

    pub fn base(&self) -> &str {
        &self.bases[self.current.load(Ordering::Relaxed) % self.bases.len()]
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub async fn call_raw(&self, name: &str, args: Value) -> Result<Value, OpError> {
        let mut last = OpError::new(ErrorKind::Unavailable, "no hub configured");
        let mut leader_hint: Option<String> = None;
        // Each base once, plus one follow-up if a standby named the leader.
        for attempt in 0..self.bases.len() + 1 {
            let base = match leader_hint.take() {
                Some(h) => h,
                None if attempt < self.bases.len() => self.base().to_string(),
                None => break,
            };
            let mut req = self.http.post(format!("{base}/v1/ops/{name}")).json(&args);
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last = OpError::new(ErrorKind::Unavailable, format!("{base}: {e}"));
                    self.current.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 503 {
                leader_hint = resp.headers().get(LEADER_HEADER).and_then(|v| v.to_str().ok()).map(String::from);
                last = OpError::new(ErrorKind::Unavailable, format!("{base} is not the leader"));
                self.current.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let body = resp.bytes().await.map_err(|e| OpError::new(ErrorKind::Unavailable, e.to_string()))?;
            if (200..300).contains(&status) {
                return serde_json::from_slice(&body).map_err(|e| OpError::internal(format!("bad response: {e}")));
            }
            return Err(serde_json::from_slice(&body)
                .unwrap_or_else(|_| OpError::new(ErrorKind::from_status(status), String::from_utf8_lossy(&body))));
        }
        Err(last)
    }

    pub async fn call<O: Op>(&self, args: &O::Args) -> Result<O::Out, OpError> {
        let v = self.call_raw(O::NAME, serde_json::to_value(args).map_err(|e| OpError::bad(e.to_string()))?).await?;
        serde_json::from_value(v).map_err(|e| OpError::internal(format!("bad response: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Auth, router};
    use crate::testing::*;
    use crate::Role;
    use axum::http::HeaderMap;
    use std::sync::Arc;

    fn auth() -> Auth<Who> {
        Arc::new(|h: HeaderMap| {
            Box::pin(async move {
                if h.get("authorization").is_some() {
                    Ok(Who(Role::Admin))
                } else {
                    Err(OpError::new(ErrorKind::Unauthorized, "token please"))
                }
            })
        })
    }

    async fn serve(app: axum::Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn typed_calls_errors_and_failover() {
        let good = serve(router(Arc::new(registry()), auth(), "t")).await;
        let c = Client::new(&good, Some("x".into()));
        assert_eq!(c.call::<Add>(&AddArgs { a: 1, b: 2 }).await.unwrap(), 3);
        assert_eq!(c.call_raw("nope", Value::Null).await.unwrap_err().kind, ErrorKind::NotFound);
        let anon = Client::new(&good, None);
        assert_eq!(anon.call::<List>(&crate::NoArgs {}).await.unwrap_err().kind, ErrorKind::Unauthorized);

        // A dead URL first, then a standby that points at the leader.
        let standby = serve(axum::Router::new().fallback({
            let good = good.clone();
            move || {
                let good = good.clone();
                async move { (axum::http::StatusCode::SERVICE_UNAVAILABLE, [(LEADER_HEADER, good)]) }
            }
        }))
        .await;
        let c = Client::new(&format!("http://127.0.0.1:1,{standby}"), Some("x".into()));
        assert_eq!(c.call::<Add>(&AddArgs { a: 2, b: 2 }).await.unwrap(), 4);
        let dead = Client::new("http://127.0.0.1:1", None);
        assert_eq!(dead.call_raw("add", Value::Null).await.unwrap_err().kind, ErrorKind::Unavailable);
    }
}
