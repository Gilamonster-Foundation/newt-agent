pub fn double_first(v: &[i32]) -> Option<i32> {
    v.first().map(|&n| n * 2)
}

pub fn double_last(v: &[i32]) -> Option<i32> {
    v.last().map(|&n| n * 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_first_works() {
        assert_eq!(double_first(&[3, 1, 4]), Some(6));
    }

    #[test]
    fn double_last_works() {
        assert_eq!(double_last(&[3, 1, 4]), Some(8));
    }
}
