// SPDX-License-Identifier: Apache-2.0
//! Helpers most control panel plugins need: the service's domain, a stable username, a password.

use sha2::{Digest, Sha256};

use crate::{Error, Service};

/// The service's domain: its `domain` field, else its label if that looks like a domain.
pub fn domain_of(service: &Service) -> Result<String, Error> {
    let d = service
        .field("domain")
        .or(Some(service.label.trim()).filter(|l| !l.is_empty()))
        .map(|d| d.trim_end_matches('.').to_lowercase())
        .ok_or_else(|| Error::refused("the service needs a domain"))?;
    let ok = d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if ok {
        Ok(d)
    } else {
        Err(Error::refused(format!("{d:?} is not a domain name")))
    }
}

/// A username made from the domain (up to `base` lower case letters and digits, starting with a
/// letter) and two characters from the service id: the same every time for a service, so a
/// repeated create finds the account it made.
pub fn username_for(domain: &str, service_id: &str, base: usize) -> String {
    let mut base: String = domain
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .skip_while(|c| !c.is_ascii_lowercase())
        .take(base)
        .collect();
    if base.is_empty() {
        base.push('u');
    }
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let hash = Sha256::digest(service_id.as_bytes());
    for b in &hash[..2] {
        base.push(ALPHABET[*b as usize % ALPHABET.len()] as char);
    }
    base
}

/// A random 20-character password with lower and upper case letters and digits.
pub fn password() -> String {
    const LOWER: &[u8] = b"abcdefghijkmnpqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGITS: &[u8] = b"23456789";
    let all: Vec<u8> = [LOWER, UPPER, DIGITS].concat();
    loop {
        let mut bytes = [0u8; 20];
        getrandom::getrandom(&mut bytes).expect("the OS random number generator");
        let p: String = bytes
            .iter()
            .map(|b| all[*b as usize % all.len()] as char)
            .collect();
        if p.chars().any(|c| c.is_ascii_lowercase())
            && p.chars().any(|c| c.is_ascii_uppercase())
            && p.chars().any(|c| c.is_ascii_digit())
        {
            return p;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    fn service(label: &str, domain: Option<&str>) -> Service {
        let mut metadata = Map::new();
        if let Some(d) = domain {
            metadata.insert("domain".into(), json!(d));
        }
        Service {
            id: "01a1".into(),
            label: label.into(),
            quantity: 1,
            status: "active".into(),
            metadata,
            provisioning: Map::new(),
        }
    }

    #[test]
    fn domains_come_from_the_field_or_the_label() {
        assert_eq!(
            domain_of(&service("x", Some("Acme.Example."))).unwrap(),
            "acme.example"
        );
        assert_eq!(
            domain_of(&service("shop.example", None)).unwrap(),
            "shop.example"
        );
        assert!(domain_of(&service("not a domain", None)).is_err());
        assert!(domain_of(&service("", None)).is_err());
        assert!(domain_of(&service("x", Some("-bad.example"))).is_err());
    }

    #[test]
    fn usernames_are_stable_and_start_with_a_letter() {
        let a = username_for("my-shop2024.example", "service-1", 8);
        assert_eq!(a, username_for("my-shop2024.example", "service-1", 8));
        assert_ne!(a, username_for("my-shop2024.example", "service-2", 8));
        assert!(a.starts_with("myshop20") && a.len() == 10);
        assert!(username_for("123.example", "s", 8).starts_with('u'));
        assert_eq!(username_for("averyverylongname.example", "s", 12).len(), 14);
    }

    #[test]
    fn passwords_mix_cases_and_digits() {
        let p = password();
        assert_eq!(p.len(), 20);
        assert_ne!(p, password());
        assert!(p.chars().any(|c| c.is_ascii_digit()));
        assert!(p.chars().any(|c| c.is_ascii_uppercase()));
    }
}
