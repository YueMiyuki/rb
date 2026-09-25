unsafe extern "C" {
    fn native_add(a: i32, b: i32) -> i32;
}

pub fn add(a: i32, b: i32) -> i32 {
    unsafe { native_add(a, b) }
}
