#[cfg_attr(feature = "json", derive(serde::Serialize))]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[cfg(feature = "json")]
pub fn to_json(p: &Point) -> String {
    serde_json::to_string(p).unwrap()
}

#[cfg(not(feature = "json"))]
pub fn to_json(p: &Point) -> String {
    format!("({}, {})", p.x, p.y)
}
