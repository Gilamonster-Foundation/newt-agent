/// Render a duration in seconds as `"<minutes>m <seconds>s"`.
pub fn humanize_duration(secs: u64) -> String {
    format!("{}m {}s", secs / 60, secs / 60)
}
