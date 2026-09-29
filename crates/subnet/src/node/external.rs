//! External executors: a process (any language) that replaces the LLM call.
//! The node keeps running the agent's state machine, tools, approvals,
//! pausing and failover; the process only produces the assistant messages.
//!
//! One process per agent type per node, speaking JSON lines on stdio:
//! - node → process: `{"t":"hello","type","id","config"}` once, then
//!   `{"t":"think","id","agent","system","messages","tools"}` and `{"t":"abort","id"}`
//! - process → node: `{"t":"delta","id","delta"}`, `{"t":"done","id"}`,
//!   `{"t":"error","id","message"}`
//!
//! A process that exits fails its open requests; the next request starts it again.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use subnet_cluster::NodeAgent;
use subnet_core::addr::AgentId;
use subnet_core::chat::{Delta, Message, ToolDef};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum FromProc {
    Delta { id: u64, delta: Delta },
    Done { id: u64 },
    Error { id: u64, message: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ToProc<'a> {
    Think { id: u64, agent: AgentId, system: &'a str, messages: &'a [Message], tools: &'a [ToolDef] },
    Abort { id: u64 },
}

type Waiters = Arc<std::sync::Mutex<HashMap<u64, mpsc::UnboundedSender<Result<Delta, String>>>>>;

struct Running {
    _child: Child,
    stdin: ChildStdin,
}

pub struct External {
    agent: NodeAgent,
    command: Vec<String>,
    proc: Mutex<Option<Running>>,
    waiters: Waiters,
    next: AtomicU64,
}

/// Aborts an unfinished request when its stream is dropped (hard pause).
struct AbortOnDrop {
    ext: Arc<External>,
    id: u64,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if self.ext.waiters.lock().unwrap().remove(&self.id).is_some() {
            let ext = self.ext.clone();
            let id = self.id;
            tokio::spawn(async move { ext.send(&ToProc::Abort { id }).await });
        }
    }
}

impl External {
    pub fn new(agent: &NodeAgent) -> Result<Self, String> {
        let command = agent.def.executor.command.clone().ok_or("not an external executor")?;
        if command.is_empty() {
            return Err("empty executor command".into());
        }
        Ok(Self {
            agent: agent.clone(),
            command,
            proc: Mutex::new(None),
            waiters: Default::default(),
            next: AtomicU64::new(1),
        })
    }

    async fn start(&self) -> Result<Running, String> {
        let (prog, args) = self.command.split_first().unwrap();
        let mut child = tokio::process::Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("starting executor {prog:?}: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let hello = json!({"t":"hello","type":self.agent.name,"id":self.agent.id,"config":self.agent.def});
        stdin.write_all(format!("{hello}\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let waiters = self.waiters.clone();
        let ty = self.agent.id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<FromProc>(&line) {
                    Ok(FromProc::Delta { id, delta }) => {
                        if let Some(tx) = waiters.lock().unwrap().get(&id) {
                            let _ = tx.send(Ok(delta));
                        }
                    }
                    Ok(FromProc::Done { id }) => {
                        waiters.lock().unwrap().remove(&id);
                    }
                    Ok(FromProc::Error { id, message }) => {
                        if let Some(tx) = waiters.lock().unwrap().remove(&id) {
                            let _ = tx.send(Err(message));
                        }
                    }
                    Err(e) => tracing::warn!(executor = %ty, error = %e, "unparseable executor output"),
                }
            }
            tracing::warn!(executor = %ty, "executor exited");
            for (_, tx) in waiters.lock().unwrap().drain() {
                let _ = tx.send(Err("executor exited".into()));
            }
        });
        Ok(Running { _child: child, stdin })
    }

    async fn send(&self, m: &ToProc<'_>) -> Result<(), String> {
        let line = format!("{}\n", serde_json::to_string(m).unwrap());
        let mut p = self.proc.lock().await;
        if let Some(r) = p.as_mut()
            && r.stdin.write_all(line.as_bytes()).await.is_ok()
        {
            return Ok(());
        }
        // Not running (or its stdin broke): start it and retry once.
        let mut r = self.start().await?;
        r.stdin.write_all(line.as_bytes()).await.map_err(|e| format!("executor stdin: {e}"))?;
        *p = Some(r);
        Ok(())
    }

    /// Asks the process for the next assistant message, as a stream of deltas.
    pub async fn think(
        self: &Arc<Self>,
        agent: AgentId,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<BoxStream<'static, Result<Delta, String>>, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.waiters.lock().unwrap().insert(id, tx);
        if let Err(e) = self.send(&ToProc::Think { id, agent, system, messages, tools }).await {
            self.waiters.lock().unwrap().remove(&id);
            return Err(e);
        }
        let guard = AbortOnDrop { ext: self.clone(), id };
        Ok(Box::pin(futures::stream::unfold((rx, guard), |(mut rx, g)| async move {
            rx.recv().await.map(|item| (item, (rx, g)))
        })))
    }
}
