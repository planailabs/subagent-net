//! `watch_agent`: a cursor long-poll over an agent's transcript, for
//! callers that can't hold a stream open (MCP tools, scripts, the CLI).

use std::time::Duration;

use subnet_core::chat::{Message, Role};
use tokio::sync::broadcast::error::RecvError;

use super::{Hub, HubError, Notice};
use crate::api::{Watch, WatchArgs, WatchCall, WatchEntry};

const DEFAULT_TIMEOUT_MS: u64 = 25_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
/// After the first event, gather what follows for this long (streamed
/// deltas come in bursts), then answer.
const COALESCE: Duration = Duration::from_millis(150);

impl Hub {
    pub async fn watch(&self, a: WatchArgs) -> Result<Watch, HubError> {
        // Subscribe before looking, so nothing between the look and the wait is missed.
        let mut rx = self.subscribe();
        let cap = a.max_chars.unwrap_or(2000);
        let t = self.transcript_of(a.id, a.full).await?;
        let len = t.messages.len() as u64;
        let timeout = a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
        let after = match a.after {
            None => return Ok(render(t, len.saturating_sub(a.tail.unwrap_or(20)), cap)),
            // Past the end: the transcript was compacted (it shrank), so
            // start over from the top of what's left.
            Some(n) if n > len => 0,
            Some(n) => n,
        };
        if len > after || timeout == 0 {
            return Ok(render(t, after, cap));
        }
        let mine = |n: &Result<Notice, RecvError>| match n {
            Ok(Notice::Agent { agent, .. }) => *agent == a.id,
            // Lagged: something may have been missed, so look again.
            Err(RecvError::Lagged(_)) => true,
            _ => false,
        };
        let wait = async {
            loop {
                let n = rx.recv().await;
                if matches!(n, Err(RecvError::Closed)) {
                    return;
                }
                if mine(&n) {
                    break;
                }
            }
            tokio::time::sleep(COALESCE).await;
        };
        let _ = tokio::time::timeout(Duration::from_millis(timeout), wait).await;
        Ok(render(self.transcript_of(a.id, a.full).await?, after, cap))
    }
}

fn cut(s: &str, cap: usize) -> (String, bool) {
    if cap == 0 || s.chars().count() <= cap {
        return (s.to_string(), false);
    }
    (format!("{}…", s.chars().take(cap).collect::<String>()), true)
}

fn entry(index: u64, m: &Message, cap: usize) -> WatchEntry {
    let mut truncated = false;
    let content = m.content.as_deref().filter(|c| !c.is_empty()).map(|c| {
        let (c, t) = cut(c, cap);
        truncated |= t;
        c
    });
    let tool_calls = m
        .tool_calls
        .iter()
        .map(|c| {
            let (arguments, t) = cut(&c.function.arguments, cap);
            truncated |= t;
            WatchCall { id: c.id.clone(), name: c.function.name.clone(), arguments }
        })
        .collect();
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    WatchEntry { index, role: role.into(), content, tool_calls, tool_call_id: m.tool_call_id.clone(), truncated }
}

fn render(t: crate::api::Transcript, from: u64, cap: usize) -> Watch {
    let entries = t.messages.iter().enumerate().skip(from as usize).map(|(i, m)| entry(i as u64, m, cap)).collect();
    let partial = t.partial.and_then(|p| p.content).filter(|c| !c.is_empty()).map(|c| cut(&c, cap).0);
    Watch { next: t.messages.len() as u64, entries, partial, queued: t.inbox.len() as u64, summary: t.summary, compacted: t.compacted }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_texts_are_cut() {
        assert_eq!(cut("héllo", 3), ("hél…".to_string(), true));
        assert_eq!(cut("héllo", 0), ("héllo".to_string(), false));
        assert_eq!(cut("hi", 3), ("hi".to_string(), false));
    }
}
