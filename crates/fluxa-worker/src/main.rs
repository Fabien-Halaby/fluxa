fn main() {
    println!("Worker started...");
    println!("Registering to scheduler... (simulated)");

    // simulation loop
    loop {
        println!("Waiting for task...");
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}
