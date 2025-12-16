use crate::entities::task::Task;

pub struct TaskRepo {
    pub tasks: Vec<Task>,
}

impl TaskRepo {
    pub fn new() -> Self {
        TaskRepo { tasks: Vec::new() }
    }

    pub fn add(&mut self, task: Task) {
        self.tasks.push(task);
    }

    pub fn get(&self, id: u64) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn all(&self) -> &Vec<Task> {
        &self.tasks
    }
}
