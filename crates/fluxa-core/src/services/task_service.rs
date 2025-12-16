use crate::entities::task::Task;

pub struct TaskService;

impl TaskService {
    pub fn new_task(id: u64, name: &str, payload: Vec<u8>) -> Task {
        Task { id, name: name.to_string(), payload }
    }

    pub fn validate_task(task: &Task) -> bool {
        !task.name.is_empty() && !task.payload.is_empty()
    }
}
