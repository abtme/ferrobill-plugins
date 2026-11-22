// SPDX-License-Identifier: Apache-2.0
//! Just enough XML for eNom's answers (`<interface-response>`): the text of an element by its
//! local name, every `<domain>` block of a listing, and the error fields (`ErrCount`,
//! `<errors><Err1>`, `<responses><response><ResponseNumber>`).
//!
//! eNom's XML is flat and regular, so a small text extractor is enough; it is the one the
//! Nominet plugin uses for EPP (`modules/nominet/src/epp.rs`), also matching `<tag/>`.

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of the first element called `local` (any namespace prefix), unescaped.
pub fn text_of(xml: &str, local: &str) -> Option<String> {
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = rest.find(|c: char| c == '>' || c.is_whitespace())?;
        let tag = rest[..end].trim_end_matches('/');
        let name = tag.rsplit(':').next().unwrap_or(tag);
        if name == local && !tag.starts_with('/') {
            let open_end = rest.find('>')?;
            if rest[..open_end].ends_with('/') {
                return Some(String::new());
            }
            let body = &rest[open_end + 1..];
            let close = body.find("</")?;
            return Some(unescape(body[..close].trim()));
        }
    }
    None
}

/// The insides of every `<local>...</local>` element (not nested in each other).
pub fn blocks<'a>(xml: &'a str, local: &str) -> Vec<&'a str> {
    let open = format!("<{local}>");
    let close = format!("</{local}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        out.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    out
}

/// An error eNom answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnomError {
    /// `Err1`, `Err2`... joined.
    pub message: String,
    /// `ResponseNumber` (six digits: 304155 bad login, 316153 domain not found, 713254 IP
    /// address not allowed, 699999 system error), if eNom sent one.
    pub number: Option<String>,
    /// `ResponseString`: e.g. `Validation error; invalid ; loginid`.
    pub kind: Option<String>,
}

impl std::fmt::Display for EnomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "eNom: {}", self.message)?;
        match (&self.number, &self.kind) {
            (Some(n), Some(k)) => write!(f, " ({n}: {k})"),
            (Some(n), None) => write!(f, " ({n})"),
            (None, Some(k)) => write!(f, " ({k})"),
            (None, None) => Ok(()),
        }
    }
}

impl EnomError {
    /// Whether trying again later could help: eNom's own system errors (`ResponseNumber` 6xxxxx,
    /// "System error"), and anything that says to try again or timed out. Not: bad logins, an
    /// address not allowed, a domain that isn't in the account, invalid values.
    pub fn retryable(&self) -> bool {
        let text = format!(
            "{} {}",
            self.message.to_ascii_lowercase(),
            self.kind.as_deref().unwrap_or("").to_ascii_lowercase()
        );
        self.number.as_deref().is_some_and(|n| n.starts_with('6'))
            || text.contains("system error")
            || text.contains("try again")
            || text.contains("timeout")
            || text.contains("timed out")
            || text.contains("temporarily")
    }
}

/// The error in an answer, if it has one: `ErrCount` above 0. `None` for a clean answer.
pub fn error(xml: &str) -> Option<EnomError> {
    let count: u32 = text_of(xml, "ErrCount")?.parse().unwrap_or(1);
    if count == 0 {
        return None;
    }
    let mut messages = Vec::new();
    for n in 1..=count.min(20) {
        if let Some(m) = text_of(xml, &format!("Err{n}")).filter(|m| !m.is_empty()) {
            messages.push(m);
        }
    }
    if messages.is_empty() {
        messages.push("an error without a message".into());
    }
    // The numbered form (`ResponseNumber1`) in text answers, the plain one inside
    // `<responses><response>` in XML ones.
    let number = text_of(xml, "ResponseNumber1").or_else(|| text_of(xml, "ResponseNumber"));
    let kind = text_of(xml, "ResponseString1").or_else(|| text_of(xml, "ResponseString"));
    Some(EnomError {
        message: messages.join("; "),
        number: number.filter(|s| !s.is_empty()),
        kind: kind.filter(|s| !s.is_empty()),
    })
}

/// eNom's `M/D/YYYY`, optionally followed by a time (`4/1/2017 7:34:00 PM`), as `YYYY-MM-DD`.
pub fn date(s: &str) -> Option<String> {
    let day_part = s.split_whitespace().next()?;
    let mut p = day_part.split('/');
    let month: u32 = p.next()?.parse().ok()?;
    let day: u32 = p.next()?.parse().ok()?;
    let year: u32 = p.next()?.parse().ok()?;
    if p.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if !(1000..=9999).contains(&year) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_by_local_name_and_blocks() {
        let x = "<interface-response><GetDomainInfo><status><expiration>4/1/2017 7:34:00 PM\
                 </expiration><registrar>eNom, Inc.</registrar></status></GetDomainInfo>\
                 <a:x>1 &amp; 2</a:x><empty/></interface-response>";
        assert_eq!(text_of(x, "expiration").unwrap(), "4/1/2017 7:34:00 PM");
        assert_eq!(text_of(x, "registrar").unwrap(), "eNom, Inc.");
        assert_eq!(text_of(x, "x").unwrap(), "1 & 2");
        assert_eq!(text_of(x, "empty").unwrap(), "");
        assert_eq!(text_of(x, "missing"), None);
        // `expiration` is not `expiration-date`.
        assert_eq!(
            text_of("<expiration-date>1/2/2030</expiration-date>", "expiration"),
            None
        );
        let l = "<domain><sld>a</sld></domain><domain><sld>b</sld></domain>";
        let b = blocks(l, "domain");
        assert_eq!(b.len(), 2);
        assert_eq!(text_of(b[1], "sld").unwrap(), "b");
    }

    #[test]
    fn dates() {
        assert_eq!(date("7/7/2012").unwrap(), "2012-07-07");
        assert_eq!(date("12/18/2027").unwrap(), "2027-12-18");
        assert_eq!(date("4/1/2017 7:34:00 PM").unwrap(), "2017-04-01");
        assert_eq!(date("2027-02-02"), None);
        assert_eq!(date("13/1/2027"), None);
        assert_eq!(date(""), None);
    }

    #[test]
    fn errors_and_whether_to_retry() {
        assert_eq!(
            error("<interface-response><ErrCount>0</ErrCount></interface-response>"),
            None
        );
        let login = error(
            "<interface-response><ErrCount>1</ErrCount><errors><Err1>Bad User name or \
             Password</Err1></errors><ResponseCount>1</ResponseCount><responses><response>\
             <ResponseNumber>304155</ResponseNumber><ResponseString>Validation error; invalid \
             ; loginid</ResponseString></response></responses></interface-response>",
        )
        .unwrap();
        assert_eq!(login.message, "Bad User name or Password");
        assert_eq!(login.number.as_deref(), Some("304155"));
        assert!(!login.retryable());
        assert!(login.to_string().contains("304155"), "{login}");
        let system = error(
            "<ErrCount>1</ErrCount><errors><Err1>An unexpected error occurred</Err1></errors>\
             <ResponseNumber>699999</ResponseNumber><ResponseString>System error; unknown; \
             unknown</ResponseString>",
        )
        .unwrap();
        assert!(system.retryable());
        let again = error("<ErrCount>1</ErrCount><Err1>Please try again later</Err1>").unwrap();
        assert!(again.retryable());
        let two = error("<ErrCount>2</ErrCount><Err1>a</Err1><Err2>b</Err2>").unwrap();
        assert_eq!(two.message, "a; b");
        assert!(!two.retryable());
    }
}
