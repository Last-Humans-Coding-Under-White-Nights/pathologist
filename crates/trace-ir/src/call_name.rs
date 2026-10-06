use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// Immutable callee spelling. Symbol tables intern these within one indexing
/// run; cloning a call record shares the text without copying its allocation.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallName(pub(crate) Arc<str>);

impl CallName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for CallName {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for CallName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for CallName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for CallName {
    fn from(name: &str) -> Self {
        Self(Arc::from(name))
    }
}

impl From<String> for CallName {
    fn from(name: String) -> Self {
        Self(name.into())
    }
}

impl PartialEq<&str> for CallName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<str> for CallName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<String> for CallName {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<CallName> for str {
    fn eq(&self, other: &CallName) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<CallName> for &str {
    fn eq(&self, other: &CallName) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<CallName> for String {
    fn eq(&self, other: &CallName) -> bool {
        self.as_str() == other.as_str()
    }
}

impl fmt::Debug for CallName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for CallName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons_accept_owned_and_borrowed_text_in_both_orders() {
        let name = CallName::from("call");
        for text in ["call", "other"] {
            let expected = text == "call";
            let owned = text.to_owned();
            assert_eq!(name == text, expected);
            assert_eq!(text == name, expected);
            assert_eq!(name == *text, expected);
            assert_eq!(*text == name, expected);
            assert_eq!(name == owned, expected);
            assert_eq!(owned == name, expected);
        }
    }
}
