//! Running hooks: the hub performs an agent's `RunHook` effects (from its own
//! replica) and commits each answer as `HookDone`. A hook that fails, times
//! out or answers nonsense is an error outcome: the agent's `on_lost` decides.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::Event;
use subnet_core::hooks::{Decision, HookPoint, HookRun, HookSpec, Outcome};

use super::{Hub, HubError};
use crate::api::Done;

/// The decisions a point takes, for a deciding agent's instructions.
fn decisions(p: HookPoint) -> &'static str {
    match p {
        HookPoint::PreTool => "\"allow\", \"deny\" (with a \"reason\" the agent reads), \"ask\" (a person decides) or \"rewrite\" (with new \"args\")",
        HookPoint::PostTool => "\"allow\" (optionally with a \"note\" added to the result), \"deny\" (the result is withheld; a \"reason\") or \"rewrite\" (a new result as \"text\")",
        HookPoint::OnMessage => "\"allow\" (optionally with a \"note\"), \"deny\" (the message is dropped) or \"rewrite\" (the message as \"text\")",
        HookPoint::OnTurnEnd => "\"allow\" (the turn ends) or \"continue\" (with a \"text\": what the agent should do next)",
        HookPoint::OnReport => "\"allow\" or \"rewrite\" (the answer as \"text\")",
        HookPoint::PreCompact => "\"allow\" (optionally with \"text\": instructions for the summary)",
        HookPoint::PreModel => "\"allow\" (with \"inject\": a list of messages it should read before this call, if any)",
    }
}

/// An outcome in a text: the JSON object in it (an agent may wrap it in words).
fn parse_outcome(text: &str) -> Result<Outcome, String> {
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else { return Err(format!("no outcome in its answer: {text:.200}")) };
    serde_json::from_str(&text[a..=b]).map_err(|e| format!("its answer isn't an outcome ({e}): {text:.200}"))
}

impl Hub {
    /// Runs a hook in the background and commits its answer.
    pub(crate) fn start_hook(self: Arc<Self>, agent: AgentId, id: String, hook: HookSpec, input: Value) {
        tokio::spawn(async move {
            let limit = Duration::from_millis(hook.timeout_ms.max(1));
            let outcome = match tokio::time::timeout(limit, self.hook_answer(agent, &hook, input)).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => Outcome::error(e),
                Err(_) => Outcome::error(format!("no answer within {} ms", hook.timeout_ms)),
            };
            if outcome.decision == Decision::Error {
                tracing::warn!(%agent, hook = %hook.name, reason = ?outcome.reason, "hook couldn't decide");
            }
            let mut st = self.st.lock().await;
            if let Err(e) = self.commit(&mut st, agent, vec![Event::HookDone { id, outcome }]).await {
                tracing::error!(%agent, hook = %hook.name, error = %e, "keeping a hook's answer failed");
            }
        });
    }

    async fn hook_answer(&self, agent: AgentId, hook: &HookSpec, input: Value) -> Result<Outcome, String> {
        let ty = self.st.lock().await.agents.get(&agent).map(|r| r.a.spec.ty.clone()).unwrap_or_default();
        let who = json!({"id": agent, "type": ty});
        if let Some(when) = &hook.when {
            let e = subnet_switchboard::Expr::compile(when)?;
            if !e.eval_bool(&[("point", &json!(hook.on.name())), ("input", &input), ("agent", &who)])? {
                return Ok(Outcome::allow());
            }
        }
        let question = json!({"hook": hook.name, "point": hook.on.name(), "agent": who, "input": input});
        match &hook.run {
            HookRun::Mcp { server, tool } => {
                // Its server's node may not be back yet (a restart): wait for it, within the timeout.
                let text = loop {
                    match self.route_mcp(server, tool, question.clone()).await {
                        Err(e) if e.contains("right now") || e.contains("no mcp") => tokio::time::sleep(Duration::from_millis(250)).await,
                        r => break r?,
                    }
                };
                parse_outcome(&text)
            }
            HookRun::Url { url } => {
                let r = reqwest::Client::new().post(url).json(&question).send().await.map_err(|e| e.to_string())?;
                let r = r.error_for_status().map_err(|e| e.to_string())?;
                parse_outcome(&r.text().await.map_err(|e| e.to_string())?)
            }
            HookRun::Spawn { mixture } => self.hook_agent(agent, hook, mixture, &question).await,
        }
    }

    /// Asks an agent to decide: its answer (a JSON outcome) comes to a
    /// mailbox of the hook run's own; the deciding agent is cancelled after.
    async fn hook_agent(&self, agent: AgentId, hook: &HookSpec, mixture: &str, question: &Value) -> Result<Outcome, String> {
        let inbox = Addr::Mailbox(format!("hook:{agent}:{}:{}", hook.name, uuid::Uuid::new_v4().simple()));
        let prompt = format!(
            "You decide for a hook ({}, at {}) of the agent {agent}: something in its work waits for your decision.\n\n{}\n\nAnswer with only a JSON object: {{\"decision\": …, \"reason\": …}} where decision is {}. Any decision may also carry \"inject\": a list of messages the agent reads before its next model call.",
            hook.name,
            hook.on.name(),
            serde_json::to_string_pretty(question).unwrap_or_default(),
            decisions(hook.on),
        );
        let s = self.spawn(&inbox, mixture, prompt).await.map_err(|e| e.to_string())?;
        let answer = loop {
            let waiting = self.mail.notified();
            let mail = self.db.take_mail(&inbox).await.map_err(|e| e.to_string())?;
            if let Some(m) = mail.into_iter().next() {
                break m.content;
            }
            waiting.await;
        };
        // It answered: its work is done.
        let _ = self.cancel(&Addr::root(), s.id).await;
        parse_outcome(&answer)
    }

    /// A person settles a hook an agent waits for (`settle_hook`): its answer,
    /// as if the hook had given it.
    pub async fn settle_hook(&self, id: AgentId, run: &str, outcome: Outcome) -> Result<Done, HubError> {
        let mut st = self.st.lock().await;
        let Some(r) = st.agents.get(&id) else { return Err(HubError::NotFound(format!("no agent {id}"))) };
        if !r.a.waiting_hooks().iter().any(|(rid, _)| rid == run) {
            return Err(HubError::Bad(format!("{id} isn't waiting for a hook run {run:?}")));
        }
        self.commit(&mut st, id, vec![Event::HookDone { id: run.into(), outcome }]).await?;
        Ok(Done::OK)
    }

    /// After a restart: agents that were waiting for hooks (and no node will
    /// recover) are recovered here, so their hooks run again or `on_lost` decides.
    pub(crate) async fn recover_hooks(&self) -> Result<(), HubError> {
        let mut st = self.st.lock().await;
        let ids: Vec<AgentId> = st.agents.iter().filter(|(_, r)| r.node.is_none() && !r.a.waiting_hooks().is_empty()).map(|(id, _)| *id).collect();
        for id in ids {
            self.commit(&mut st, id, vec![Event::Recovered]).await?;
        }
        Ok(())
    }
}
