use std::env;

use fluxa_core::services::task_service::TaskService;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() == 2 && args[1] == "test-protocol" {
        let task = TaskService::new_task(1, "Hello", vec![1,2,3]);
        println!("Task created: {:?}", task);

        let valid = TaskService::validate_task(&task);
        println!("Is task valid? {}", valid);
        return;
    }

    if args.len() > 1 {
        match args[1].as_str() {
            "submit" => println!("Submitting task..."),
            "agent" => println!("Starting agent..."),
            _ => println!("Unknown command"),
        }
    } else {
        println!("Usage: fluxa-cli [submit|agent|test-protocol]");
    }
}
