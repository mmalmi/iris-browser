//! Tree visibility helpers

use std::str::FromStr;

/// Tree visibility modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeVisibility {
    Public,
    LinkVisible,
    Private,
}

impl TreeVisibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            TreeVisibility::Public => "public",
            TreeVisibility::LinkVisible => "link-visible",
            TreeVisibility::Private => "private",
        }
    }
}

impl FromStr for TreeVisibility {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "public" => Ok(TreeVisibility::Public),
            "link-visible" | "link_visible" | "linkvisible" => Ok(TreeVisibility::LinkVisible),
            "private" => Ok(TreeVisibility::Private),
            _ => Err(format!("invalid visibility: {}", s)),
        }
    }
}

/// XOR two 32-byte keys (used for link-visible key masking)
pub fn xor_keys(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visibility_parse() {
        assert_eq!(
            TreeVisibility::from_str("public").unwrap(),
            TreeVisibility::Public
        );
        assert_eq!(
            TreeVisibility::from_str("link-visible").unwrap(),
            TreeVisibility::LinkVisible
        );
        assert_eq!(
            TreeVisibility::from_str("link_visible").unwrap(),
            TreeVisibility::LinkVisible
        );
        assert_eq!(
            TreeVisibility::from_str("private").unwrap(),
            TreeVisibility::Private
        );
        assert!(TreeVisibility::from_str("unknown").is_err());
    }

    #[test]
    fn test_xor_keys_roundtrip() {
        let a = [0x11u8; 32];
        let b = [0x22u8; 32];
        let masked = xor_keys(&a, &b);
        let unmasked = xor_keys(&masked, &b);
        assert_eq!(unmasked, a);
    }
}
