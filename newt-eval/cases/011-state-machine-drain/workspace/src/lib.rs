/// Sum positive numbers up to the first 0 sentinel.
pub fn sum_until_zero(xs: &[i32]) -> i32 {
    let mut sum = 0;
    for &x in xs {
        if x == 0 {
            break;
        }
        if x < 0 {
            return sum;
        }
        sum += x;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_at_zero() {
        assert_eq!(sum_until_zero(&[1, 2, 0, 5]), 3);
    }
}
