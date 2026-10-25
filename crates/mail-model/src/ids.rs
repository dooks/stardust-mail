//! Identifier newtypes. All serialize transparently (as the bare string / number).

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wrap a string.
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// Borrow the inner string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// A Google account, identified by the OpenID `sub` claim (stable, unlike the e-mail address).
    AccountId
);
string_id!(
    /// A Gmail message id (hex string).
    MessageId
);
string_id!(
    /// A Gmail thread id.
    ThreadId
);
string_id!(
    /// A Gmail label id (`INBOX`, `Label_12`, ...). The pseudo id `ALL` stands for "All Mail" (no label filter).
    LabelId
);

/// A MIME part index as assigned by `mail-parser`; stable for the same raw bytes. Used in asset URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartId(pub u32);

impl fmt::Display for PartId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Gmail mailbox history cursor (the API sends it as a decimal string; parse it into this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HistoryId(pub u64);

impl fmt::Display for HistoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_transparently() {
        assert_eq!(serde_json::to_string(&AccountId::new("1234")).unwrap(), "\"1234\"");
        assert_eq!(serde_json::to_string(&PartId(3)).unwrap(), "3");
        assert_eq!(serde_json::to_string(&HistoryId(77)).unwrap(), "77");
        let m: MessageId = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(m.as_str(), "abc");
        assert_eq!(m.to_string(), "abc");
    }
}
