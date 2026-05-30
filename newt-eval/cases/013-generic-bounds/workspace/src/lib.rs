pub fn largest(list: &[i32]) -> i32 {
    let mut max = list[0];
    for &x in list {
        if x > max {
            max = x;
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_int() {
        assert_eq!(largest(&[3, 7, 2, 9, 4]), 9);
    }
}
