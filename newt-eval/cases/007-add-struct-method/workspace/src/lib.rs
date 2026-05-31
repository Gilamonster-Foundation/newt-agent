pub struct Counter {
    pub count: i32,
}

impl Counter {
    pub fn new() -> Self {
        Counter { count: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_zero() {
        assert_eq!(Counter::new().count, 0);
    }
}
