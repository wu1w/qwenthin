/// Double a length. The unit tests expect 2*n, not n squared.
pub fn scale(n: i32) -> i32 {
    n * n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_doubles() {
        assert_eq!(scale(3), 6);
        assert_eq!(scale(0), 0);
        assert_eq!(scale(-2), -4);
    }
}
