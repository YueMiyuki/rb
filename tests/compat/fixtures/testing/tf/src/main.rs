fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("{}", tf::add(1, 2));
    } else {
        println!("args: {}", args.join(" "));
    }
}

#[test]
fn bin_unit_test() {
    assert_eq!(tf::ANSWER, 42);
}
