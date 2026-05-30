pub enum Color {
    Red,
    Green,
    Blue,
    Custom(u8, u8, u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_exist() {
        let _ = [Color::Red, Color::Green, Color::Blue, Color::Custom(1, 2, 3)];
    }
}
