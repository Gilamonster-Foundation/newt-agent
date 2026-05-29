pub fn cross_host_greet(name: &str) -> String {
    format!("Hello, {name}!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets() {
        assert_eq!(cross_host_greet("a"), "Hello, a!");
    }
}
