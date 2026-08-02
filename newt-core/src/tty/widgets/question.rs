use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action<A> {
    pub value: A,
    pub key: String,
    pub label: String,
}

impl<A> Action<A> {
    pub fn new(value: A, key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value,
            key: key.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question<A> {
    pub markdown: String,
    pub actions: Vec<Action<A>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl<A: AsRef<str> + Clone> Question<A> {
    pub fn parse(&self, input: &str) -> Option<A> {
        let input = input.trim();
        self.actions
            .iter()
            .find(|a| a.key == input || a.value.as_ref() == input)
            .map(|a| a.value.clone())
    }
}

impl<A> Question<A> {
    pub fn terminal_text(&self) -> String {
        let choices = self
            .actions
            .iter()
            .map(|a| a.label.replacen(&a.key, &format!("[{}]", a.key), 1))
            .collect::<Vec<_>>()
            .join("   ");
        [&self.markdown, self.note.as_deref().unwrap_or(""), &choices]
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
