use std::thread;
use std::time::Duration;

fn main() {
    println!("Scheduler started...");

    // simulation loop
    loop {
        println!("Checking workers, tasks...");
        thread::sleep(Duration::from_secs(5));
    }
}
