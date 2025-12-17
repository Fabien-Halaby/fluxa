use std::io::{self};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use mini_cluster_proto::mini_cluster::worker_service_client::WorkerServiceClient;
use mini_cluster_proto::mini_cluster::{StatusRequest, TaskRequest};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
struct StatusModel {
    node_name: String,
    cpu_usage: f32,
    free_ram_bytes: u64,
    current_task: String,
    last_log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:50051".to_string());

    let (status_tx, mut status_rx) = mpsc::channel::<StatusModel>(100);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(10);

    // Task pour écouter StreamStatus
    let target_clone = target.clone();
    tokio::spawn(async move {
        loop {
            match WorkerServiceClient::connect(target_clone.clone()).await {
                Ok(mut client) => {
                    let request = tonic::Request::new(StatusRequest {});
                    if let Ok(mut stream) = client.stream_status(request).await {
                        let mut model = StatusModel::default();
                        while let Some(Ok(update)) = stream.get_mut().next().await {
                            model.node_name = update.node_name;
                            model.cpu_usage = update.cpu_usage;
                            model.free_ram_bytes = update.free_ram_bytes;
                            model.current_task = update.current_task;
                            if !update.log_line.is_empty() {
                                model.last_log = update.log_line;
                            }
                            let _ = status_tx.send(model.clone()).await;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error connecting status stream: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // Task pour envoyer des commandes (SubmitTask)
    let target_clone2 = target.clone();
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match WorkerServiceClient::connect(target_clone2.clone()).await {
                Ok(mut client) => {
                    let id = Uuid::new_v4().to_string();
                    let req = TaskRequest {
                        id: id.clone(),
                        command: cmd.clone(),
                        workdir: ".".into(),
                    };
                    let request = tonic::Request::new(req);
                    match client.submit_task(request).await {
                        Ok(resp) => {
                            let r = resp.into_inner();
                            println!("Task submitted: {} ({})", r.id, r.message);
                        }
                        Err(e) => {
                            eprintln!("Failed to submit task: {e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to connect for submit: {e}");
                }
            }
        }
    });

    // TUI
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut current_status = StatusModel::default();
    let mut input = String::new();

    loop {
        // Met à jour le modèle si on a des nouvelles
        while let Ok(status) = status_rx.try_recv() {
            current_status = status;
        }

        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints(
                    [
                        Constraint::Length(3),
                        Constraint::Length(3),
                        Constraint::Min(3),
                    ]
                    .as_ref(),
                )
                .split(area);
            
            let header_text = vec![Line::from(vec![
                Span::raw("Target: "),
                Span::styled(&target, Style::default().fg(Color::Cyan)),
            ])];
            let header = Paragraph::new(header_text)
                .block(Block::default().borders(Borders::ALL).title("Mini Cluster Client"));
            f.render_widget(header, chunks[0]);
            
            let status_text = vec![
                Line::from(format!(
                    "Node: {} | CPU: {:.1}% | Free RAM: {:.2} MiB | Task: {}",
                    current_status.node_name,
                    current_status.cpu_usage,
                    current_status.free_ram_bytes as f64 / (1024.0 * 1024.0),
                    current_status.current_task
                )),
                Line::from(format!("Last log: {}", current_status.last_log)),
            ];
            let status_widget = Paragraph::new(status_text)
                .block(Block::default().borders(Borders::ALL).title("Status"));
            f.render_widget(status_widget, chunks[1]);
            
            // ⬇⬇⬇ MODIF ICI
            let input_text: String = input.clone();
            let input_widget = Paragraph::new(input_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Command to run on worker (Enter to submit, q to quit)"),
                );
            f.render_widget(input_widget, chunks[2]);
        })?;

        if crossterm::event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Enter => {
                        let cmd = input.trim().to_string();
                        if !cmd.is_empty() {
                            let _ = cmd_tx.send(cmd).await;
                        }
                        input.clear();
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
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
