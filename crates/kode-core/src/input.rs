use serde::{Deserialize, Serialize};

/// One image supplied with a user message. `data` is standard base64 without
/// a data-URL prefix; providers add their own wire-specific envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub name: String,
    pub media_type: String,
    pub data: String,
    pub size_bytes: usize,
}

/// Provider-agnostic user input shared by the TUI, pipeline, agent, session
/// store, and deferred-steering path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    #[serde(default)]
    pub images: Vec<ImageAttachment>,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    pub fn append(&mut self, mut other: Self) {
        if !other.text.trim().is_empty() {
            if !self.text.is_empty() {
                self.text.push_str("\n\n");
            }
            self.text.push_str(&other.text);
        }
        self.images.append(&mut other.images);
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<&UserInput> for UserInput {
    fn from(value: &UserInput) -> Self {
        value.clone()
    }
}

impl std::ops::Deref for UserInput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_merges_text_and_images_without_losing_order_within_each_kind() {
        let mut input = UserInput::text("first");
        input.images.push(ImageAttachment {
            name: "one.png".into(),
            media_type: "image/png".into(),
            data: "AA==".into(),
            size_bytes: 1,
        });
        input.append(UserInput {
            text: "second".into(),
            images: vec![ImageAttachment {
                name: "two.jpg".into(),
                media_type: "image/jpeg".into(),
                data: "AQ==".into(),
                size_bytes: 1,
            }],
        });

        assert_eq!(input.text, "first\n\nsecond");
        assert_eq!(input.images.len(), 2);
        assert_eq!(input.images[1].name, "two.jpg");
    }
}
