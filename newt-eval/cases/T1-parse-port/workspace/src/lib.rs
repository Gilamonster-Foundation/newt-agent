/// Parse a port number from a string.
pub fn parse_port(s: &str) -> u16 {
    s.parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid() {
        assert_eq!(parse_port("8080"), 8080);
    }
}
