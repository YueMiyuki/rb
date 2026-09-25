fn main() {
    println!("2 + 40 = {} (header seen: {})", native_sys::add(2, 40), cfg!(has_native_header));
}
