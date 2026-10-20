//! `looper-sharepoint run`'s terminal UI. It only renders [`Service::status`] / [`Service::history`] and calls the
//! service's public API; it has no access to engine internals.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use looper_sharepoint_engine::{Health, Mode, Outcome, PushState, RunRecord, Service, Status, Trigger};
use tokio::sync::mpsc;

use crate::fmt::{ago, failure_text, first_line, mode_line, push_text, run_line, short_duration, state_text, until};

pub async fn run(svc: Service, log_file: &std::path::Path) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &svc, log_file).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, svc: &Service, log_file: &std::path::Path) -> Result<()> {
    // crossterm's reader blocks, so it gets its own thread.
    let (key_tx, mut keys) = mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || {
        while !key_tx.is_closed() {
            if event::poll(Duration::from_millis(200)).unwrap_or(false)
                && let Ok(TermEvent::Key(k)) = event::read()
                && k.kind == KeyEventKind::Press
                && key_tx.send(k).is_err()
            {
                break;
            }
        }
    });
    let mut events = svc.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut table = TableState::default().with_selected(0);
    let mut flash: Option<String> = None;
    loop {
        let status = svc.status();
        let n = status.libraries.len();
        let history = svc.history(100);
        terminal.draw(|f| draw(f, &status, &history, &mut table, flash.as_deref(), log_file))?;
        tokio::select! {
            k = keys.recv() => {
                let Some(k) = k else { return Ok(()) };
                let selected = table.selected().and_then(|i| status.libraries.get(i)).map(|l| l.name.clone());
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Up | KeyCode::Char('k') => table.select(Some(table.selected().unwrap_or(0).saturating_sub(1))),
                    KeyCode::Down | KeyCode::Char('j') if n > 0 => table.select(Some((table.selected().unwrap_or(0) + 1).min(n - 1))),
                    KeyCode::Char('s') | KeyCode::Enter => {
                        if let Some(name) = selected {
                            svc.trigger(Some(&name), Trigger::Manual)?;
                            flash = Some(format!("sync requested for {name}"));
                        }
                    }
                    KeyCode::Char('a') => {
                        svc.trigger(None, Trigger::Manual)?;
                        flash = Some("sync requested for all libraries".into());
                    }
                    KeyCode::Char('r') => {
                        svc.reconnect_push(None)?;
                        flash = Some("reconnecting live updates".into());
                    }
                    KeyCode::Char('p') => {
                        if status.paused {
                            svc.resume();
                            flash = Some("resumed".into());
                        } else {
                            svc.pause();
                            flash = Some("paused: scheduled syncs stopped (manual syncs still run)".into());
                        }
                    }
                    _ => {}
                }
            }
            // Any service event means "redraw"; drain bursts (e.g. per-file progress) into one frame.
            _ = events.recv() => {
                while events.try_recv().is_ok() {}
            }
            _ = tick.tick() => {}
        }
    }
}

fn draw(f: &mut Frame, s: &Status, history: &[RunRecord], table: &mut TableState, flash: Option<&str>, log_file: &std::path::Path) {
    let now = Utc::now();
    let [header, mode, libs, detail, runs, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(s.libraries.len() as u16 + 3),
        Constraint::Length(6),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let health_color = match s.health() {
        Health::Ok => Color::Green,
        Health::Syncing => Color::Cyan,
        Health::Waiting => Color::Yellow,
        Health::Attention => Color::Red,
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" looper-sharepoint ", Style::new().bold()),
            Span::raw(format!(
                "─ {} ─ up {} ─ requests {} (throttled {}) ─ ",
                s.account.as_deref().unwrap_or("not signed in"),
                short_duration((now - s.started_at).num_seconds()),
                s.requests,
                s.throttled_responses
            )),
            Span::styled(s.headline(), Style::new().fg(health_color).bold()),
        ])),
        header,
    );

    // The mode bar: full width, coloured, so "live" vs "polling" is visible at a glance.
    let mode_style = match s.mode() {
        Mode::Live => Style::new().fg(Color::Black).bg(Color::Green),
        Mode::PartlyLive { .. } | Mode::Polling { .. } => Style::new().fg(Color::Black).bg(Color::Yellow),
        Mode::Connecting => Style::new().fg(Color::Black).bg(Color::Cyan),
        Mode::Off => Style::new().add_modifier(Modifier::REVERSED),
    };
    f.render_widget(Paragraph::new(format!(" {}", mode_line(s, now))).style(mode_style.bold()), mode);

    let rows = s.libraries.iter().map(|l| {
        let state = state_text(l, s.paused);
        let color = match l.state {
            looper_sharepoint_engine::LibraryState::Idle => Color::Green,
            looper_sharepoint_engine::LibraryState::Syncing { .. } => Color::Cyan,
            _ => Color::Red,
        };
        let push_color = match l.push {
            PushState::Live { .. } => Color::Green,
            PushState::Off => Color::Reset,
            PushState::Connecting { .. } => Color::Cyan,
            _ => Color::Yellow,
        };
        Row::new(vec![
            Cell::from(l.name.clone()),
            Cell::from(state).style(Style::new().fg(color)),
            Cell::from(push_text(l, now)).style(Style::new().fg(push_color)),
            Cell::from(ago(l.last_success_at, now)),
            Cell::from(failure_text(l, now)),
            Cell::from(if s.paused { "paused".into() } else { until(l.next_run_at, now) }),
        ])
    });
    let t = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(20),
            Constraint::Length(30),
            Constraint::Length(10),
            Constraint::Fill(1),
            Constraint::Length(11),
        ],
    )
    .header(
        Row::new(["LIBRARY", "STATE", "LIVE UPDATES", "LAST SYNC", "LAST FAILURE", "NEXT CHECK"])
            .style(Style::new().add_modifier(Modifier::DIM)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ")
    .block(Block::new().borders(Borders::TOP).title(format!(
        " libraries (checked every {} while polling, every {} while live) ",
        short_duration(s.poll_interval_secs as i64),
        short_duration(s.push_safety_interval_secs as i64)
    )));
    f.render_stateful_widget(t, libs, table);

    // Details for the selected library: where it lives, the full last error, item-level problems.
    let mut lines = vec![];
    if let Some(l) = table.selected().and_then(|i| s.libraries.get(i)) {
        lines.push(Line::from(format!("{}  →  {}", l.site, l.local_path)).dim());
        let mut push = format!("local changes: {} · live updates: {}", crate::fmt::local_changes_text(l), push_text(l, now));
        if let Some(t) = l.last_push_at {
            push.push_str(&format!(" · last change notification {}", ago(Some(t), now)));
        }
        lines.push(Line::from(push));
        if let Some(fail) = &l.last_failure {
            lines.push(Line::from(format!("last failure {}: {}", ago(Some(fail.at), now), first_line(&fail.error))).red());
        }
        if let Some(run) = &l.last_run {
            for i in run.issues.iter().take(2) {
                lines.push(Line::from(format!("! {}: {}", i.path, i.reason)).yellow());
            }
        }
        if let Some(t) = s.throttled_until {
            lines.push(Line::from(format!("SharePoint is throttling: requests wait until {}", crate::fmt::local_time(t))).yellow());
        }
        if l.state == looper_sharepoint_engine::LibraryState::Refused {
            lines.push(Line::from(format!("refused: if the deletions are intended, run `looper-sharepoint sync --library {} --allow-mass-delete`", l.name)).magenta());
        }
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::new().borders(Borders::TOP).title(" details ")),
        detail,
    );

    let items: Vec<ListItem> = history
        .iter()
        .take(runs.height as usize)
        .map(|r| {
            let color = match r.outcome {
                Outcome::Ok => Color::Reset,
                Outcome::Problems => Color::Yellow,
                Outcome::Failed => Color::Red,
                Outcome::Refused => Color::Magenta,
            };
            ListItem::new(run_line(r)).style(Style::new().fg(color))
        })
        .collect();
    f.render_widget(List::new(items).block(Block::new().borders(Borders::TOP).title(" recent runs ")), runs);

    let keys = " s sync selected · a sync all · r reconnect live updates · p pause/resume · ↑↓ select · q quit";
    let text = match flash {
        Some(m) => format!("{keys}   │ {m}"),
        None => format!("{keys}   │ log: {}", log_file.display()),
    };
    f.render_widget(Paragraph::new(text).dim(), footer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as D;
    use ratatui::backend::TestBackend;
    use looper_sharepoint_engine::{Failure, LibraryState, LibraryStatus, Summary};

    fn lib(name: &str, state: LibraryState) -> LibraryStatus {
        let now = Utc::now();
        LibraryStatus {
            name: name.into(),
            local_path: format!("/Users/me/SharePoint/{name}"),
            site: "https://contoso.sharepoint.com/sites/eng".into(),
            library: "Documents".into(),
            state,
            last_success_at: Some(now - D::seconds(12)),
            last_failure: Some(Failure { at: now - D::hours(3), error: "503 Service Unavailable".into() }),
            next_run_at: Some(now + D::seconds(30)),
            consecutive_failures: 0,
            last_run: None,
            push: PushState::Live { since: now - D::minutes(134) },
            last_push_at: Some(now - D::seconds(14)),
            watching: true,
            uploads: true,
        }
    }

    /// Renders a representative screen; `cargo test -p looper-sharepoint tui -- --nocapture` prints it.
    #[test]
    fn renders_libraries_and_history() {
        let now = Utc::now();
        let status = Status {
            account: Some("spqa1@sethcall.com".into()),
            tenant: "t".into(),
            started_at: now - D::minutes(134),
            paused: false,
            throttled_until: None,
            requests: 1204,
            throttled_responses: 0,
            ru_estimate: 0,
            requests_by_kind: Default::default(),
            poll_interval_secs: 45,
            push_safety_interval_secs: 900,
            libraries: vec![
                lib("qa", LibraryState::Idle),
                LibraryStatus {
                    push: PushState::Down {
                        since: now - D::minutes(2),
                        reason: "no heartbeat from server for 50s".into(),
                        retry_at: Some(now + D::seconds(11)),
                    },
                    ..lib("legal", LibraryState::Syncing { trigger: Trigger::Manual, started_at: now, done: 340, total: 1003 })
                },
            ],
        };
        let run = |lib: &str, secs: i64, outcome: Outcome, error: Option<&str>, downloaded: usize| RunRecord {
            library: lib.into(),
            trigger: Trigger::Poll,
            started_at: now - D::seconds(secs),
            finished_at: now - D::seconds(secs) + D::milliseconds(800),
            outcome,
            error: error.map(str::to_string),
            summary: error.is_none().then(|| Summary { downloaded, ..Default::default() }),
            issues: vec![],
        };
        let history = vec![
            run("qa", 10, Outcome::Ok, None, 2),
            run("qa", 55, Outcome::Ok, None, 0),
            run("legal", 120, Outcome::Failed, Some("sign-in expired; run `looper-sharepoint login`"), 0),
        ];
        let mut term = ratatui::Terminal::new(TestBackend::new(130, 20)).unwrap();
        let mut table = TableState::default().with_selected(0);
        term.draw(|f| draw(f, &status, &history, &mut table, None, std::path::Path::new("/tmp/log"))).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .chunks(130)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
        assert!(text.contains("spqa1@sethcall.com"));
        assert!(text.contains("syncing 340/1003"));
        assert!(text.contains("12s ago"));
        assert!(text.contains("3h00m ago: 503 Service Unavailable"));
        assert!(text.contains("sign-in expired"));
        assert!(text.contains("2 downloaded"));
        assert!(text.contains("◐ PARTLY LIVE · 1 of 2 libraries live"));
        assert!(text.contains("● live for 2h14m"));
        assert!(text.contains("◐ down 2m: no heartbeat"));
        assert!(text.contains("last change notification 14s ago"));
    }
}
