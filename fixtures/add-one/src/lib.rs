/// Returns `n + 1`. The unit test below is currently failing on purpose.
pub fn add_one(n: i32) -> i32 {
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_one() {
        assert_eq!(add_one(1), 2);
        assert_eq!(add_one(-1), 0);
    }
}
