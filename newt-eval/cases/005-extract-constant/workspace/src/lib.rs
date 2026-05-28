pub fn seconds_in(days: u64) -> u64 {
    60 * 60 * 24 * days
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_day() {
        assert_eq!(seconds_in(1), 86_400);
    }
}
