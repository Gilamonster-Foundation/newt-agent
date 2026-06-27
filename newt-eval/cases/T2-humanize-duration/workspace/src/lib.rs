pub mod format;
pub mod util;

pub use util::humanize_duration;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes() {
        assert_eq!(humanize_duration(90), "1m 30s");
    }
}
