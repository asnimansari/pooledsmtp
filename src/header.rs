//! A minimal, ordered, multi-value MIME header map that mirrors Go's
//! `net/textproto.MIMEHeader` closely enough for building and inspecting
//! e-mail messages. Keys are canonicalized (e.g. `content-type` ->
//! `Content-Type`) so lookups are case-insensitive, exactly like Go.

/// Canonicalizes a MIME header key the way Go's
/// `textproto.CanonicalMIMEHeaderKey` does: the first letter and any letter
/// following a `-` are upper-cased, everything else is lower-cased.
/// E.g. `MESSAGE-id` -> `Message-Id`.
pub fn canonicalize(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = true;
    for c in key.chars() {
        if upper {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        upper = c == '-';
    }
    out
}

/// An ordered, multi-value header map with canonicalized keys.
#[derive(Clone, Debug, Default)]
pub struct MimeHeader {
    entries: Vec<(String, Vec<String>)>,
}

impl MimeHeader {
    pub fn new() -> Self {
        MimeHeader::default()
    }

    fn position(&self, canon: &str) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == canon)
    }

    /// Replaces any existing values for `key` with the single value `val`.
    pub fn set(&mut self, key: &str, val: &str) {
        let canon = canonicalize(key);
        match self.position(&canon) {
            Some(i) => self.entries[i].1 = vec![val.to_string()],
            None => self.entries.push((canon, vec![val.to_string()])),
        }
    }

    /// Appends `val` to the list of values for `key`.
    pub fn add(&mut self, key: &str, val: &str) {
        let canon = canonicalize(key);
        match self.position(&canon) {
            Some(i) => self.entries[i].1.push(val.to_string()),
            None => self.entries.push((canon, vec![val.to_string()])),
        }
    }

    /// Returns the first value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<&str> {
        let canon = canonicalize(key);
        self.position(&canon)
            .and_then(|i| self.entries[i].1.first())
            .map(|s| s.as_str())
    }

    /// Returns all values for `key`, if the key is present.
    pub fn get_all(&self, key: &str) -> Option<&[String]> {
        let canon = canonicalize(key);
        self.position(&canon).map(|i| self.entries[i].1.as_slice())
    }

    /// Reports whether `key` is present.
    pub fn contains(&self, key: &str) -> bool {
        let canon = canonicalize(key);
        self.position(&canon).is_some()
    }

    /// Iterates over `(canonical_key, values)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Vec<String>)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }
}
