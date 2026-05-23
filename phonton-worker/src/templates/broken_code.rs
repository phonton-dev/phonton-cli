fn get_percentage(value: u32, total: u32) -> f32 {
    let ratio = value as f32 / total as f32;
    ratio * 100.0
}
