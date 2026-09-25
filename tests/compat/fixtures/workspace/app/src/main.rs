use macros::Hello;

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

#[derive(Hello)]
struct Greeter;

fn main() {
    let point = core_lib::Point { x: 1, y: 2 };
    println!("{}", Greeter::hello());
    println!("json: {}", core_lib::to_json(&point));
    println!("build script: cfg={} env={} generated={GENERATED}", cfg!(built_by_script), env!("BUILD_GREETING"));
}
