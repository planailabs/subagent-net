//! `subnet tui`: the agent park and an agent pane in the terminal, on the same
//! API and event stream as the web UI.

use std::collections::HashMap;
use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use serde_json::{Value, json};
use subnet_core::addr::AgentId;
use tokio::sync::mpsc;

use crate::api::{AgentSummary, Transcript};
use crate::client::{Client, tail};

/// One line of the park: a plot header or an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Plot { title: String },
    Agent { id: AgentId, depth: usize, last: bool },
}

/// What a key asks the outside world to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Quit,
    /// An operation on the selected agent (its id is added).
    Op(&'static str, Value),
    Send(String),
}

#[derive(Default)]
pub struct App {
    pub agents: HashMap<AgentId, AgentSummary>,
    pub rows: Vec<Row>,
    /// Index into `rows` (always an agent row when any exists).
    pub selected: usize,
    pub transcript: Option<Transcript>,
    /// Streaming text per agent.
    pub live: HashMap<AgentId, String>,
    /// A message being typed (`m`).
    pub input: Option<String>,
    pub status: String,
}

pub fn glyph(a: &AgentSummary) -> &'static str {
    match a.phase.as_str() {
        "cancelled" => "·",
        "failed" => "✕",
        _ if a.paused => "‖",
        _ if a.awaiting_approval.is_some() => "?",
        "thinking" => "●",
        "tools" => "▣",
        _ => "○",
    }
}

fn type_name(ty: &str) -> &str {
    ty.split('@').next().unwrap_or(ty)
}

fn short(id: &AgentId) -> String {
    id.to_string()[..4].to_string()
}

fn bar(a: &AgentSummary, width: usize) -> String {
    let Some(max) = a.budget.max_tokens.filter(|m| *m > 0) else { return "·".repeat(width) };
    let used = a.usage.prompt_tokens + a.usage.completion_tokens + a.reserved;
    let full = ((used as f64 / max as f64).min(1.0) * width as f64).round() as usize;
    "█".repeat(full) + &"░".repeat(width - full)
}

impl App {
    /// Replaces the agent list, rebuilding plots and keeping the selection.
    pub fn set_agents(&mut self, list: Vec<AgentSummary>) {
        let keep = self.selected_id();
        self.agents = list.iter().map(|a| (a.id, a.clone())).collect();
        let mut kids: HashMap<AgentId, Vec<&AgentSummary>> = HashMap::new();
        let mut roots = vec![];
        for a in &list {
            match a.parent.filter(|p| self.agents.contains_key(p)) {
                Some(p) => kids.entry(p).or_default().push(a),
                None => roots.push(a),
            }
        }
        let live = |a: &AgentSummary| matches!(a.phase.as_str(), "thinking" | "tools") && !a.paused;
        roots.sort_by(|x, y| live(y).cmp(&live(x)).then(x.id.cmp(&y.id)));
        self.rows.clear();
        for r in roots {
            self.rows.push(Row::Plot { title: format!("{} {}", type_name(&r.ty), short(&r.id)) });
            let mut stack = vec![(r, 0usize, true)];
            while let Some((a, depth, last)) = stack.pop() {
                self.rows.push(Row::Agent { id: a.id, depth, last });
                let mut ks = kids.get(&a.id).cloned().unwrap_or_default();
                ks.sort_by_key(|k| std::cmp::Reverse(k.id));
                let n = ks.len();
                for (i, k) in ks.into_iter().enumerate() {
                    stack.push((k, depth + 1, i == 0 && n > 0));
                }
            }
        }
        self.selected = keep
            .and_then(|id| self.rows.iter().position(|r| matches!(r, Row::Agent { id: x, .. } if *x == id)))
            .or_else(|| self.rows.iter().position(|r| matches!(r, Row::Agent { .. })))
            .unwrap_or(0);
    }

    pub fn selected_id(&self) -> Option<AgentId> {
        match self.rows.get(self.selected) {
            Some(Row::Agent { id, .. }) => Some(*id),
            _ => None,
        }
    }

    fn move_by(&mut self, d: isize) {
        let mut i = self.selected as isize;
        loop {
            i += d;
            if i < 0 || i as usize >= self.rows.len() {
                return;
            }
            if matches!(self.rows[i as usize], Row::Agent { .. }) {
                self.selected = i as usize;
                self.transcript = None;
                return;
            }
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        if k.kind != KeyEventKind::Press {
            return Action::None;
        }
        if let Some(input) = &mut self.input {
            match k.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    let text = self.input.take().unwrap_or_default();
                    return if text.trim().is_empty() { Action::None } else { Action::Send(text) };
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return Action::None;
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        let approval = self.selected_id().and_then(|id| self.agents.get(&id)).and_then(|a| a.awaiting_approval.clone());
        match k.code {
            KeyCode::Esc => Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_by(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_by(-1);
                Action::None
            }
            KeyCode::Char('s') => Action::Op("pause", json!({"mode": "safe"})),
            KeyCode::Char('q') => Action::Op("pause", json!({"mode": "quick"})),
            KeyCode::Char('h') => Action::Op("pause", json!({"mode": "hard"})),
            KeyCode::Char('r') => Action::Op("resume", json!({})),
            KeyCode::Char('x') => Action::Op("cancel", json!({})),
            KeyCode::Char('f') => Action::Op("fork", json!({})),
            KeyCode::Char(c @ ('a' | 'd')) => match approval {
                Some(call) => Action::Op("approve", json!({"call_id": call.id, "approved": c == 'a'})),
                None => {
                    self.status = "nothing to approve".into();
                    Action::None
                }
            },
            KeyCode::Char('m') if self.selected_id().is_some() => {
                self.input = Some(String::new());
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Folds a notice from the event stream; true if summaries are stale.
    pub fn on_notice(&mut self, n: &Value) -> bool {
        if n["kind"] != "agent" {
            return false;
        }
        let Ok(id) = n["agent"].as_str().unwrap_or_default().parse::<AgentId>() else { return false };
        match n["event"]["type"].as_str() {
            Some("llm_delta") => {
                if let Some(c) = n["event"]["delta"]["content"].as_str() {
                    let t = self.live.entry(id).or_default();
                    t.push_str(c);
                    if t.len() > 4000 {
                        *t = t[t.len() - 2000..].to_string();
                    }
                }
                false
            }
            Some("llm_done") => {
                self.live.remove(&id);
                true
            }
            _ => true,
        }
    }
}

fn last_line(s: &str) -> &str {
    s.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

pub fn draw(f: &mut Frame, app: &App) {
    let [main, foot] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|r| match r {
            Row::Plot { title } => ListItem::new(Line::styled(format!("── {title} "), dim)),
            Row::Agent { id, depth, last } => {
                let a = &app.agents[id];
                let prefix = if *depth == 0 { String::new() } else { "  ".repeat(depth - 1) + if *last { "└ " } else { "├ " } };
                let text = app.live.get(id).map(|t| last_line(t)).or(a.last.as_deref().map(last_line)).unwrap_or("");
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(prefix, dim),
                        Span::styled(format!("{} ", glyph(a)), bold),
                        Span::raw(format!("{} ", type_name(&a.ty))),
                        Span::styled(format!("{} ", short(id)), dim),
                        Span::styled(bar(a, 6), dim),
                    ]),
                    Line::styled(format!("    {}", text.chars().take(60).collect::<String>()), dim),
                ])
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected));
    let park = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(" park · {} agents ", app.agents.len())))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(park, left, &mut state);

    let mut lines: Vec<Line> = vec![];
    if let Some(t) = &app.transcript {
        let s = &t.summary;
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", glyph(s)), bold),
            Span::styled(type_name(&s.ty).to_string(), bold),
            Span::styled(format!("  {}", s.id), dim),
        ]));
        let pause = s.pause.map(|p| format!(" · pause {p:?}").to_lowercase()).unwrap_or_default();
        lines.push(Line::styled(
            format!(
                "{}{pause} · node {} · {} tokens{}",
                s.phase,
                s.node.as_deref().unwrap_or("–"),
                s.usage.total(),
                if s.usage.cached_prompt_tokens > 0 { format!(" ({} cached)", s.usage.cached_prompt_tokens) } else { String::new() }
            ),
            dim,
        ));
        if let Some(c) = &s.awaiting_approval {
            lines.push(Line::styled(format!("? approve {}({})  [a]pprove / [d]eny", c.function.name, c.function.arguments), bold));
        }
        for m in &t.messages {
            let role = serde_json::to_value(m.role).unwrap();
            lines.push(Line::raw(""));
            lines.push(Line::styled(role.as_str().unwrap_or_default().to_string(), dim));
            let body = match (&m.content, m.tool_calls.is_empty()) {
                (Some(c), _) => c.clone(),
                (None, false) => m.tool_calls.iter().map(|c| format!("→ {}({})", c.function.name, c.function.arguments)).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            };
            lines.extend(body.lines().map(|l| Line::raw(l.to_string())));
        }
        let streaming = app.live.get(&s.id).cloned().or_else(|| t.partial.as_ref().and_then(|p| p.content.clone()));
        if let Some(p) = streaming {
            lines.push(Line::raw(""));
            lines.push(Line::styled("assistant · streaming", dim));
            lines.extend(p.lines().map(|l| Line::raw(l.to_string())));
        }
    } else {
        lines.push(Line::styled("select an agent (j/k)", dim));
    }
    // Show the end of the transcript.
    let height = right.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(height);
    let pane = Paragraph::new(lines.split_off(skip))
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" agent "));
    f.render_widget(pane, right);

    let footer = match &app.input {
        Some(t) => Line::from(vec![Span::styled("message: ", bold), Span::raw(t.clone()), Span::raw("▏")]),
        None if !app.status.is_empty() => Line::styled(app.status.clone(), dim),
        None => Line::styled(
            "j/k select · s/q/h pause safe/quick/hard · r resume · x cancel · a/d approve/deny · m message · f fork · esc quit",
            dim,
        ),
    };
    f.render_widget(Paragraph::new(footer), foot);
}

enum Msg {
    Key(KeyEvent),
    Notice(Value),
    Tick,
}

pub async fn run(c: Client, hub: String, token: Option<String>) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let keys = tx.clone();
    // Terminal input on a thread: crossterm's reads block.
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => {
                    if let Ok(Event::Key(k)) = event::read()
                        && keys.send(Msg::Key(k)).is_err()
                    {
                        return;
                    }
                }
                Ok(false) => {
                    if keys.send(Msg::Tick).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    let notices = tx.clone();
    tokio::spawn(async move {
        loop {
            let n = notices.clone();
            let _ = tail(&hub, token.as_deref(), None, false, move |v| {
                let _ = n.send(Msg::Notice(v));
            })
            .await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    let mut term = ratatui::init();
    let mut app = App::default();
    let mut stale = true;
    let mut last_fetch = std::time::Instant::now() - Duration::from_secs(10);
    let result = loop {
        if stale || last_fetch.elapsed() > Duration::from_secs(5) {
            match c.call_raw("list_agents", Value::Null).await.and_then(|v| {
                serde_json::from_value::<Vec<AgentSummary>>(v).map_err(|e| subnet_ops::OpError::internal(e.to_string()))
            }) {
                Ok(list) => app.set_agents(list),
                Err(e) => app.status = format!("hub: {e}"),
            }
            if let Some(id) = app.selected_id()
                && let Ok(v) = c.call_raw("transcript", json!({"id": id})).await
            {
                app.transcript = serde_json::from_value(v).ok();
            }
            stale = false;
            last_fetch = std::time::Instant::now();
        }
        if let Err(e) = term.draw(|f| draw(f, &app)) {
            break Err(e.into());
        }
        let Some(msg) = rx.recv().await else { break Ok(()) };
        match msg {
            Msg::Tick => {}
            Msg::Notice(n) => stale |= app.on_notice(&n),
            Msg::Key(k) => {
                let before = app.selected_id();
                match app.on_key(k) {
                    Action::Quit => break Ok(()),
                    Action::None => stale |= app.selected_id() != before,
                    Action::Op(op, mut args) => {
                        let Some(id) = app.selected_id() else { continue };
                        args["id"] = json!(id);
                        app.status = match c.call_raw(op, args).await {
                            Ok(v) if op == "fork" => format!("forked as {}", v["id"].as_str().unwrap_or("?")),
                            Ok(_) => format!("{op}: ok"),
                            Err(e) => format!("{op}: {e}"),
                        };
                        stale = true;
                    }
                    Action::Send(text) => {
                        let Some(id) = app.selected_id() else { continue };
                        app.status = match c.call_raw("send", json!({"to": format!("agent:{id}"), "content": text})).await {
                            Ok(_) => "sent".into(),
                            Err(e) => format!("send: {e}"),
                        };
                        stale = true;
                    }
                }
            }
        }
    };
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use subnet_core::agent::Budget;
    use subnet_core::chat::{ToolCall, Usage};

    fn agent(n: u128, parent: Option<u128>, phase: &str) -> AgentSummary {
        AgentSummary {
            id: AgentId::from_u128(n),
            ty: "worker@abc".into(),
            parent: parent.map(AgentId::from_u128),
            phase: phase.into(),
            pause: None,
            paused: false,
            node: None,
            usage: Usage { prompt_tokens: 30, completion_tokens: 20, ..Default::default() },
            budget: Budget { max_tokens: Some(100), ..Default::default() },
            reserved: 0,
            compactions: 0,
            tenant: None,
            seq: 1,
            awaiting_approval: None,
            last: Some("all done".into()),
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn plots_and_selection() {
        let mut app = App::default();
        app.set_agents(vec![agent(1, None, "idle"), agent(2, Some(1), "idle"), agent(3, Some(1), "idle"), agent(9, None, "thinking")]);
        assert_eq!(app.rows.len(), 6, "two plot headers, four agents");
        assert!(matches!(&app.rows[0], Row::Plot { .. }));
        assert!(matches!(app.rows[1], Row::Agent { id, depth: 0, .. } if id == AgentId::from_u128(9)), "live plot first");
        assert_eq!(app.selected_id(), Some(AgentId::from_u128(9)));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_id(), Some(AgentId::from_u128(1)), "headers are skipped");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_id(), Some(AgentId::from_u128(2)));
        // The selection survives a refresh.
        app.set_agents(vec![agent(1, None, "idle"), agent(2, Some(1), "idle"), agent(3, Some(1), "idle"), agent(9, None, "idle")]);
        assert_eq!(app.selected_id(), Some(AgentId::from_u128(2)));
    }

    #[test]
    fn keys_map_to_operations() {
        let mut app = App::default();
        app.set_agents(vec![agent(1, None, "thinking")]);
        assert_eq!(app.on_key(key(KeyCode::Char('h'))), Action::Op("pause", json!({"mode":"hard"})));
        assert_eq!(app.on_key(key(KeyCode::Char('r'))), Action::Op("resume", json!({})));
        assert_eq!(app.on_key(key(KeyCode::Char('a'))), Action::None, "nothing to approve");
        let mut a = agent(1, None, "tools");
        a.awaiting_approval = Some(ToolCall::new("c7", "rm", "{}"));
        app.set_agents(vec![a]);
        assert_eq!(app.on_key(key(KeyCode::Char('d'))), Action::Op("approve", json!({"call_id":"c7","approved":false})));
        // Typing a message.
        app.on_key(key(KeyCode::Char('m')));
        for c in "hi!".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Send("hi".into()));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Quit);
    }

    #[test]
    fn notices_stream_text() {
        let mut app = App::default();
        let id = AgentId::from_u128(1);
        let delta = json!({"kind":"agent","agent":id,"event":{"type":"llm_delta","delta":{"content":"hel"}}});
        assert!(!app.on_notice(&delta));
        app.on_notice(&delta);
        assert_eq!(app.live[&id], "helhel");
        assert!(app.on_notice(&json!({"kind":"agent","agent":id,"event":{"type":"llm_done"}})));
        assert!(!app.live.contains_key(&id));
        assert!(!app.on_notice(&json!({"kind":"sense"})));
    }

    #[test]
    fn renders_park_and_pane() {
        let mut app = App::default();
        app.set_agents(vec![agent(1, None, "thinking"), agent(2, Some(1), "idle")]);
        app.live.insert(AgentId::from_u128(1), "streaming words".into());
        let mut term = ratatui::Terminal::new(TestBackend::new(120, 20)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        for want in ["park · 2 agents", "● worker", "└ ○ worker", "███░░░", "streaming words", "all done", "select an agent"] {
            assert!(screen.contains(want), "missing {want:?}");
        }
    }
}
