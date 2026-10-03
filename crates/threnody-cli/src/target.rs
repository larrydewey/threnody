//! Parsing of "who to dial": invite links, contacts, or raw addresses.

use anyhow::{Result, bail};
use threnody_core::Fingerprint;
use threnody_core::store::{Contacts, Lookup};

const SCHEME: &str = "threnody://";

/// `threnody://<fingerprint>@<host:port>`
pub fn invite_link(fp: &Fingerprint, addr: &str) -> String {
    format!("{SCHEME}{}@{addr}", fp.compact())
}

/// Resolves to `(address, pinned fingerprint)`.
pub fn resolve(target: &str, contacts: &Contacts) -> Result<(String, Option<Fingerprint>)> {
    if let Some(rest) = target.strip_prefix(SCHEME) {
        let Some((fp, addr)) = rest.split_once('@') else {
            bail!("invite link must look like {SCHEME}<fingerprint>@<host:port>");
        };
        return Ok((addr.to_owned(), Some(fp.parse()?)));
    }
    match contacts.find(target) {
        Lookup::Found(c) => match &c.last_addr {
            Some(a) => Ok((a.clone(), Some(c.fingerprint()))),
            None => bail!(
                "no known address for {}; connect with an invite link or host:port",
                c.label()
            ),
        },
        Lookup::Ambiguous(_) => bail!("{target:?} matches several contacts"),
        Lookup::None if target.contains(':') => Ok((target.to_owned(), None)),
        Lookup::None => bail!("{target:?} is not an invite link, contact, or host:port"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use threnody_core::Identity;

    #[test]
    fn invite_links_round_trip() {
        let fp = Identity::generate().public().fingerprint();
        let link = invite_link(&fp, "192.0.2.1:7450");
        let (addr, pin) = resolve(&link, &Contacts::default()).unwrap();
        assert_eq!(addr, "192.0.2.1:7450");
        assert_eq!(pin, Some(fp));
        assert_eq!(
            resolve("[::1]:7450", &Contacts::default()).unwrap(),
            ("[::1]:7450".into(), None)
        );
        assert!(resolve("nobody", &Contacts::default()).is_err());
    }
}
