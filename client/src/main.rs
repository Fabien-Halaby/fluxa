use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use mini_cluster_proto::mini_cluster::worker_service_client::WorkerServiceClient;
use mini_cluster_proto::mini_cluster::{ProjectChunk, StatusRequest, TaskRequest, UploadAck};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Terminal,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use uuid::Uuid;

const MAX_LOG_LINES: usize = 2000;
const TICK_RATE: Duration = Duration::from_millis(33);
const PROMPT: &str = "fluxa> ";

#[derive(Clone, Debug, Default)]
struct NodeStatus {
    node_name: String,
    cpu_usage: f32,
    free_ram_bytes: u64,
    current_task: String,
    connected: bool,
    last_seen: Option<Instant>,
}

#[derive(Clone, Debug)]
enum Focus {
    Input,
    Output,
}

#[derive(Clone, Debug)]
struct App {
    target: String,
    status: NodeStatus,

    logs: VecDeque<String>,
    follow_tail: bool,
    scroll: u16,
    focus: Focus,

    input: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    message: String,
}

impl App {
    fn new(target: String) -> Self {
        Self {
            target,
            status: NodeStatus::default(),
            logs: VecDeque::new(),
            follow_tail: true,
            scroll: 0,
            focus: Focus::Input,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            message: "Ready".into(),
        }
    }

    fn push_log(&mut self, line: String) {
        if line.is_empty() {
            return;
        }
        self.logs.push_back(line);
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }
        if self.follow_tail {
            self.scroll_to_bottom();
        }
    }

    fn output_height(&self, output_area: Rect) -> usize {
        output_area.height.saturating_sub(2) as usize
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = u16::MAX;
        self.follow_tail = true;
    }

    fn set_follow_tail(&mut self, follow: bool) {
        self.follow_tail = follow;
        if follow {
            self.scroll_to_bottom();
        }
    }

    fn clamp_scroll(&mut self, output_area: Rect) {
        let visible = self.output_height(output_area);
        let total = self.logs.len();
        let max_scroll = total.saturating_sub(visible);
        let max_scroll_u16 = max_scroll.min(u16::MAX as usize) as u16;
        if self.scroll > max_scroll_u16 {
            self.scroll = max_scroll_u16;
        }
    }

    fn scroll_up(&mut self, n: u16) {
        self.follow_tail = false;
        self.focus = Focus::Output;
        self.scroll = self.scroll.saturating_sub(n);
    }

    fn scroll_down(&mut self, n: u16) {
        self.follow_tail = false;
        self.focus = Focus::Output;
        self.scroll = self.scroll.saturating_add(n);
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.cursor - 1;
        self.input.remove(prev);
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        self.input.remove(self.cursor);
    }

    fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.input.len();
    }

    fn commit_history(&mut self, cmd: &str) {
        if cmd.trim().is_empty() {
            return;
        }
        if self.history.last().map(|s| s.as_str()) != Some(cmd) {
            self.history.push(cmd.to_string());
        }
        self.history_idx = None;
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.history_idx = Some(match self.history_idx {
            None => self.history.len().saturating_sub(1),
            Some(i) => i.saturating_sub(1),
        });
        if let Some(i) = self.history_idx {
            self.input = self.history[i].clone();
            self.cursor = self.input.len();
        }
    }

    fn history_down(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {}
            Some(i) => {
                let next = i + 1;
                if next >= self.history.len() {
                    self.history_idx = None;
                    self.input.clear();
                    self.cursor = 0;
                } else {
                    self.history_idx = Some(next);
                    self.input = self.history[next].clone();
                    self.cursor = self.input.len();
                }
            }
        }
    }
}

#[derive(Debug)]
enum AppEvent {
    Tick,
    Key(KeyEvent),
    Resize,
    StatusUpdate(NodeStatus, Option<String>),
    ClientInfo(String),
}

fn start_event_thread(tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || {
        let mut last_tick = Instant::now();
        loop {
            let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
            if event::poll(timeout).unwrap_or(false) {
                match event::read() {
                    Ok(CEvent::Key(k)) => {
                        let _ = tx.send(AppEvent::Key(k));
                    }
                    Ok(CEvent::Resize(_, _)) => {
                        let _ = tx.send(AppEvent::Resize);
                    }
                    _ => {}
                }
            }
            if last_tick.elapsed() >= TICK_RATE {
                let _ = tx.send(AppEvent::Tick);
                last_tick = Instant::now();
            }
        }
    });
}

async fn start_status_task(target: String, tx: mpsc::UnboundedSender<AppEvent>) {
    loop {
        match WorkerServiceClient::connect(target.clone()).await {
            Ok(mut client) => {
                let mut st = NodeStatus::default();
                st.connected = true;
                st.last_seen = Some(Instant::now());
                let _ = tx.send(AppEvent::StatusUpdate(st.clone(), Some("Connected".into())));

                let req = tonic::Request::new(StatusRequest {});
                match client.stream_status(req).await {
                    Ok(mut stream) => {
                        while let Some(msg) = stream.get_mut().next().await {
                            match msg {
                                Ok(update) => {
                                    st.node_name = update.node_name;
                                    st.cpu_usage = update.cpu_usage;
                                    st.free_ram_bytes = update.free_ram_bytes;
                                    st.current_task = update.current_task;
                                    st.connected = true;
                                    st.last_seen = Some(Instant::now());

                                    let log = if update.log_line.is_empty() {
                                        None
                                    } else {
                                        Some(update.log_line)
                                    };
                                    let _ = tx.send(AppEvent::StatusUpdate(st.clone(), log));
                                }
                                Err(e) => {
                                    let mut s = NodeStatus::default();
                                    s.connected = false;
                                    let _ = tx.send(AppEvent::StatusUpdate(
                                        s,
                                        Some(format!("Disconnected: {e}")),
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let mut s = NodeStatus::default();
                        s.connected = false;
                        let _ = tx.send(AppEvent::StatusUpdate(
                            s,
                            Some(format!("stream_status failed: {e}")),
                        ));
                    }
                }
            }
            Err(e) => {
                let mut s = NodeStatus::default();
                s.connected = false;
                let _ = tx.send(AppEvent::StatusUpdate(
                    s,
                    Some(format!("connect failed: {e}")),
                ));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn submit_command(target: String, cmd: String, tx: mpsc::UnboundedSender<AppEvent>) -> Result<()> {
    let mut client: WorkerServiceClient<Channel> = WorkerServiceClient::connect(target).await?;
    let id = Uuid::new_v4().to_string();
    let req = TaskRequest {
        id: id.clone(),
        command: cmd.clone(),
        workdir: ".".into(),
    };
    let res = client.submit_task(tonic::Request::new(req)).await?.into_inner();
    let msg = format!("submitted {} accepted={} ({})", res.id, res.accepted, res.message);
    let _ = tx.send(AppEvent::ClientInfo(msg));
    Ok(())
}

async fn upload_project(
    target: String,
    job_id: String,
    archive_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    let mut client: WorkerServiceClient<Channel> = WorkerServiceClient::connect(target).await?;

    let data = tokio::fs::read(&archive_path).await?;
    let total = data.len();

    // IMPORTANT: Stream<Item = ProjectChunk> (pas Result<...>)
    let chunk_vec: Vec<ProjectChunk> = data
        .chunks(64 * 1024)
        .map(|chunk| ProjectChunk {
            job_id: job_id.clone(),
            data: chunk.to_vec(),
        })
        .collect();

    let stream = tokio_stream::iter(chunk_vec);

    let response = client
        .upload_project(tonic::Request::new(stream))
        .await?;

    let ack: UploadAck = response.into_inner();
    let msg = format!(
        "upload job={} ok={} ({}) total={} bytes",
        ack.job_id, ack.ok, ack.message, total
    );
    let _ = tx.send(AppEvent::ClientInfo(msg));
    Ok(())
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();

    let root = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled("Fluxa", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  •  Target: "),
        Span::styled(app.target.as_str(), Style::default().fg(Color::Yellow)),
    ])])
    .block(Block::default().borders(Borders::ALL).title("Remote Build / Exec"));
    f.render_widget(header, root[0]);

    let conn = if app.status.connected { "connected" } else { "disconnected" };
    let cpu = format!("{:.1}%", app.status.cpu_usage);
    let ram = format!("{:.2} MiB", app.status.free_ram_bytes as f64 / (1024.0 * 1024.0));
    let task = if app.status.current_task.is_empty() { "idle" } else { app.status.current_task.as_str() };

    let status_lines = vec![Line::from(format!(
        "Node: {} | {} | CPU: {} | Free RAM: {} | Task: {}",
        if app.status.node_name.is_empty() { "-" } else { app.status.node_name.as_str() },
        conn,
        cpu,
        ram,
        task
    ))];

    let status = Paragraph::new(status_lines).block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(status, root[1]);

    let output_area = root[2];
    app.clamp_scroll(output_area);

    let log_lines: Vec<Line> = app.logs.iter().map(|l| Line::from(l.as_str())).collect();

    let title = match app.focus {
        Focus::Output => "Output (scroll mode)",
        Focus::Input => "Output",
    };

    let output = Paragraph::new(log_lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((app.scroll, 0));
    f.render_widget(output, output_area);

    let visible = app.output_height(output_area);
    let total = app.logs.len();
    let max_scroll = total.saturating_sub(visible);
    let scroll_pos = app.scroll.min(max_scroll.min(u16::MAX as usize) as u16) as usize;

    let mut sb_state = ScrollbarState::default()
        .content_length(total.max(1))
        .position(scroll_pos);

    let sb = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);

    f.render_stateful_widget(sb, output_area, &mut sb_state);

    let input_block_title = match app.focus {
        Focus::Input => "Command (edit mode)",
        Focus::Output => "Command",
    };

    let input_line = Line::from(vec![
        Span::styled(PROMPT, Style::default().fg(Color::Green)),
        Span::raw(app.input.as_str()),
    ]);

    let input = Paragraph::new(vec![input_line])
        .block(Block::default().borders(Borders::ALL).title(input_block_title));
    f.render_widget(input, root[3]);

    if matches!(app.focus, Focus::Input) {
        let x = root[3].x + 1 + PROMPT.len() as u16 + app.cursor as u16;
        let y = root[3].y + 1;
        f.set_cursor_position((x, y));
    }

    let follow = if app.follow_tail { "tail" } else { "free" };
    let help = "Enter: run | upload <id> <tar.gz> | Tab: focus | PgUp/PgDn: scroll | End: tail | Ctrl+L: clear | q: quit";
    let bar = Line::from(vec![
        Span::styled(
            format!(" {} ", app.message),
            Style::default().fg(Color::Black).bg(Color::White),
        ),
        Span::raw(" "),
        Span::styled(format!("[{}]", follow), Style::default().fg(Color::Magenta)),
        Span::raw(" "),
        Span::styled(help, Style::default().fg(Color::DarkGray)),
    ]);
    let bar_widget = Paragraph::new(vec![bar]);
    f.render_widget(bar_widget, root[4]);
}

#[tokio::main]
async fn main() -> Result<()> {
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:50051".to_string());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(target.clone());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AppEvent>();

    start_event_thread(ev_tx.clone());
    tokio::spawn(start_status_task(target.clone(), ev_tx.clone()));

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        match ev_rx.recv().await {
            None => break,
            Some(AppEvent::Resize) => terminal.autoresize()?,
            Some(AppEvent::Tick) => {
                if app.follow_tail {
                    app.scroll_to_bottom();
                }
            }
            Some(AppEvent::ClientInfo(msg)) => app.message = msg,
            Some(AppEvent::StatusUpdate(st, log)) => {
                app.status = st;
                if let Some(line) = log {
                    app.push_log(line);
                }
            }
            Some(AppEvent::Key(k)) => {
                if k.modifiers.is_empty() && k.code == KeyCode::Char('q') {
                    break;
                }

                if k.modifiers.contains(KeyModifiers::CONTROL) {
                    match k.code {
                        KeyCode::Char('l') => {
                            app.logs.clear();
                            app.scroll = 0;
                            app.set_follow_tail(true);
                            app.message = "cleared output".into();
                        }
                        KeyCode::Char('c') => {
                            app.push_log("[client] Ctrl+C pressed (cancel not implemented yet)".into());
                        }
                        _ => {}
                    }
                    continue;
                }

                if k.code == KeyCode::Tab {
                    app.focus = match app.focus {
                        Focus::Input => Focus::Output,
                        Focus::Output => Focus::Input,
                    };
                    app.message = if matches!(app.focus, Focus::Input) { "edit mode" } else { "scroll mode" }.into();
                    continue;
                }

                if matches!(app.focus, Focus::Output) {
                    match k.code {
                        KeyCode::Esc => {
                            app.focus = Focus::Input;
                            app.message = "edit mode".into();
                        }
                        KeyCode::Up => app.scroll_up(1),
                        KeyCode::Down => app.scroll_down(1),
                        KeyCode::PageUp => app.scroll_up(10),
                        KeyCode::PageDown => app.scroll_down(10),
                        KeyCode::End => {
                            app.set_follow_tail(true);
                            app.focus = Focus::Input;
                            app.message = "tail mode".into();
                        }
                        _ => {}
                    }
                    continue;
                }

                match k.code {
                    KeyCode::Enter => {
                        let cmd = app.input.trim().to_string();
                        if cmd.is_empty() {
                            continue;
                        }

                        app.push_log(format!("{}{}", PROMPT, &cmd));
                        app.commit_history(&cmd);
                        app.input.clear();
                        app.cursor = 0;

                        let tx = ev_tx.clone();
                        let tgt = app.target.clone();

                        // Special command: upload <job_id> <archive_path>
                        if let Some(rest) = cmd.strip_prefix("upload ") {
                            let parts: Vec<&str> = rest.split_whitespace().collect();
                            if parts.len() == 2 {
                                let job_id = parts[0].to_string();
                                let archive_path = parts[1].to_string();
                                tokio::spawn(async move {
                                    if let Err(e) = upload_project(tgt, job_id, archive_path, tx.clone()).await {
                                        let _ = tx.send(AppEvent::ClientInfo(format!("upload failed: {e}")));
                                    }
                                });
                            } else {
                                let _ = tx.send(AppEvent::ClientInfo("usage: upload <job_id> <archive.tar.gz>".into()));
                            }
                        } else {
                            tokio::spawn(async move {
                                if let Err(e) = submit_command(tgt, cmd, tx.clone()).await {
                                    let _ = tx.send(AppEvent::ClientInfo(format!("submit failed: {e}")));
                                }
                            });
                        }
                    }
                    KeyCode::Char(c) => app.insert_char(c),
                    KeyCode::Backspace => app.backspace(),
                    KeyCode::Delete => app.delete(),
                    KeyCode::Left => app.cursor_left(),
                    KeyCode::Right => app.cursor_right(),
                    KeyCode::Home => app.home(),
                    KeyCode::End => app.end(),
                    KeyCode::Up => app.history_up(),
                    KeyCode::Down => app.history_down(),
                    KeyCode::PageUp => {
                        app.focus = Focus::Output;
                        app.scroll_up(10);
                        app.message = "scroll mode".into();
                    }
                    KeyCode::PageDown => {
                        app.focus = Focus::Output;
                        app.scroll_down(10);
                        app.message = "scroll mode".into();
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
