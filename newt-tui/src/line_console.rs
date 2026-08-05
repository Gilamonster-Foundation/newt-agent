use std::io::{self, Write};
pub trait Console {
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    fn say(&mut self, line: &str);
}
pub struct StdinConsole;
impl Console for StdinConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buf = String::new();
        let n = io::stdin().read_line(&mut buf)?;
        if n == 0 {
            return Ok(String::new());
        }
        Ok(buf.trim().to_string())
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}
pub fn is_yes(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => false,
    }
}
