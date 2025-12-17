use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hostname;
use mini_cluster_proto::mini_cluster::worker_service_server::{
    WorkerService, WorkerServiceServer,
};
use mini_cluster_proto::mini_cluster::{
    StatusRequest, StatusUpdate, TaskRequest, TaskSubmitted,
};
use sysinfo::System;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};

#[derive(Clone, Debug)]
struct CurrentState {
    node_name: String,
    cpu_usage: f32,
    free_ram_bytes: u64,
    current_task: Option<String>,
}

#[derive(Clone)]
struct SharedState {
    state: Arc<RwLock<CurrentState>>,
    log_tx: mpsc::Sender<String>,
    log_rx: Arc<Mutex<mpsc::Receiver<String>>>,
}

struct WorkerServiceImpl {
    shared: SharedState,
}

type StatusStream =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<StatusUpdate, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    async fn submit_task(
        &self,
        request: Request<TaskRequest>,
    ) -> Result<Response<TaskSubmitted>, Status> {
        let req = request.into_inner();
        let id = req.id.clone();
        let command = req.command.clone();
        let workdir = req.workdir.clone();

        {
            let mut state = self.shared.state.write().await;
            state.current_task = Some(format!("{}: {}", id, command));
        }

        let log_tx = self.shared.log_tx.clone();
        let state_clone = self.shared.state.clone();

        tokio::spawn(async move {
            if let Err(e) = run_command(id.clone(), command, workdir, log_tx.clone()).await {
                let _ = log_tx
                    .send(format!("task {id} failed: {e:?}"))
                    .await;
            }

            let mut st = state_clone.write().await;
            st.current_task = None;
        });

        let reply = TaskSubmitted {
            id: req.id,
            accepted: true,
            message: "Task accepted".into(),
        };

        Ok(Response::new(reply))
    }

    type StreamStatusStream = StatusStream;

    async fn stream_status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<Self::StreamStatusStream>, Status> {
        let (tx, rx) = mpsc::channel::<Result<StatusUpdate, Status>>(100);
        let shared = self.shared.clone();

        tokio::spawn(async move {
            loop {
                let state = shared.state.read().await.clone();

                let log_line = {
                    let mut rx = shared.log_rx.lock().await;
                    match rx.recv().await {
                        Some(line) => line,
                        None => String::new(),
                    }
                };

                let update = StatusUpdate {
                    node_name: state.node_name.clone(),
                    cpu_usage: state.cpu_usage,
                    free_ram_bytes: state.free_ram_bytes,
                    current_task: state.current_task.clone().unwrap_or_default(),
                    log_line,
                };

                if tx.send(Ok(update)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as Self::StreamStatusStream))
    }
}

async fn run_command(
    id: String,
    command: String,
    workdir: String,
    log_tx: mpsc::Sender<String>,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&command).current_dir(workdir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(stdout) = stdout {
        let log_tx_clone = log_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = log_tx_clone.send(line).await;
            }
        });
    }

    if let Some(stderr) = stderr {
        let log_tx_clone = log_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = log_tx_clone
                    .send(format!("[stderr] {line}"))
                    .await;
            }
        });
    }

    let status = child.wait().await?;
    let _ = log_tx
        .send(format!("command {id} exited with: {status}"))
        .await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let node_name = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let sys = Mutex::new(System::new_all());
    let (log_tx, log_rx) = mpsc::channel::<String>(100);
    let shared = SharedState {
        state: Arc::new(RwLock::new(CurrentState {
            node_name: node_name.clone(),
            cpu_usage: 0.0,
            free_ram_bytes: 0,
            current_task: None,
        })),
        log_tx: log_tx.clone(),
        log_rx: Arc::new(Mutex::new(log_rx)),
    };

    // Task pour mettre à jour CPU/RAM
    let shared_clone = shared.clone();
    tokio::spawn(async move {
        loop {
            {
                let mut s = sys.lock().await;
                s.refresh_cpu();
                s.refresh_memory();

                let cpu_usage = if s.cpus().is_empty() {
                    0.0
                } else {
                    s.cpus()
                        .iter()
                        .map(|c| c.cpu_usage())
                        .sum::<f32>()
                        / s.cpus().len() as f32
                };

                let free_ram = s.available_memory(); // bytes

                let mut state = shared_clone.state.write().await;
                state.cpu_usage = cpu_usage;
                state.free_ram_bytes = free_ram;
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    });

    let addr = "0.0.0.0:50051".parse()?;
    let svc = WorkerServiceImpl { shared };

    println!("Worker on {node_name} listening on {addr}");
    Server::builder()
        .add_service(WorkerServiceServer::new(svc))
        .serve(addr)
        .await?;

    Ok(())
}
