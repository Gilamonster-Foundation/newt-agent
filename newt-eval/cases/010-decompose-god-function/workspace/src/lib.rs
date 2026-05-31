pub fn summarize(nums: &[i32]) -> String {
    let mut sum = 0;
    for n in nums {
        sum += n;
    }
    let mut max = i32::MIN;
    for n in nums {
        if *n > max {
            max = *n;
        }
    }
    let count = nums.len();
    format!("count={count} sum={sum} max={max}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes() {
        assert_eq!(summarize(&[3, 1, 4, 1, 5]), "count=5 sum=14 max=5");
    }
}
