fn main() {
    let bench = std::env::args().any(|a| a == "--bench");
    let mut sum = 0u64;
    for i in 0..1000 {
        sum += u64::from(tf::add(i, 1));
    }
    println!("speed: sum={sum} bench-mode={bench}");
}
