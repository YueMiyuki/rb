#[derive(serde::Serialize)]
struct Tick {
    n: u32,
    runtime: &'static str,
}

#[tokio::main]
async fn main() {
    let handle = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        Tick { n: 1, runtime: "tokio" }
    });
    let tick = handle.await.unwrap();
    println!("{}", serde_json::to_string(&tick).unwrap());
}
