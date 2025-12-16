use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Task {
    pub id: u64,
    pub name: String,
    pub payload: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskResult {
    pub task_id: u64,
    pub result: Vec<u8>,
    pub success: bool,
}
