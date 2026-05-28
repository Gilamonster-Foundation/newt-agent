pub fn double(n: i32) -> i32 {
    n * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_two() {
        assert_eq!(double(2), 4);
    }
}
