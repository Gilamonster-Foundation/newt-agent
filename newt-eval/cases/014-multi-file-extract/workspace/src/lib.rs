pub fn area(w: u32, h: u32) -> u32 {
    multiply(w, h)
}

fn multiply(a: u32, b: u32) -> u32 {
    a * b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_area() {
        assert_eq!(area(3, 4), 12);
    }
}
