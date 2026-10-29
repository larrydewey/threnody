//! Parsing of "who to dial": invite links, contacts, or raw addresses.

use anyhow::{Result, bail};
use threnody_core::Fingerprint;
use threnody_core::store::{Contacts, Lookup};

use threnody_core::invite::{InviteLink, SCHEME};

/// `threnody://<fingerprint>@<host:port>`, with no key: for links that
/// must not lead anywhere but that address (an anonymous identity's).
pub fn invite_link(fp: &Fingerprint, addr: &str) -> String {
    format!("{SCHEME}{}@{addr}", fp.compact())
}

/// The first address in an invite link, if `target` is one.
pub fn invite_addr(target: &str) -> Option<String> {
    target
        .parse::<InviteLink>()
        .ok()
        .and_then(|l| l.addrs.into_iter().next())
}

/// Resolves to `(address, pinned fingerprint)`.
pub fn resolve(target: &str, contacts: &Contacts) -> Result<(String, Option<Fingerprint>)> {
    if target.starts_with(SCHEME) {
        let link: InviteLink = target.parse()?;
        let Some(addr) = link.addrs.into_iter().next() else {
            bail!("this invite has no address; /connect looks its maker up instead");
        };
        return Ok((addr, Some(link.fingerprint)));
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
