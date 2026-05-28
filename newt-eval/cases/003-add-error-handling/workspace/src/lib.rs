pub fn parse(s: &str) -> i32 {
    s.parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_int() {
        assert_eq!(parse("42"), 42);
    }
}
