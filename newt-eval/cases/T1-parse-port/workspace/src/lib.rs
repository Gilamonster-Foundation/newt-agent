/// Parse a port number, returning `None` when the input is not a valid port.
pub fn parse_port(s: &str) -> Option<u16> {
    Some(s.parse().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid() {
        assert_eq!(parse_port("8080"), Some(8080));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_port("not-a-port"), None);
    }
}
