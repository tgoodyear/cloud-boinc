//! Addresses the site never mails: those that cannot receive mail by
//! definition.
//!
//! A message to such an address is certain to bounce, and bounces count
//! against the sending reputation that every volunteer of a project shares.
//! [`undeliverable`] names the cases that need no DNS lookup and no guess:
//!
//! - a name reserved by RFC 2606 and RFC 6761: `example.com`, `example.net`
//!   and `example.org` with every subdomain, and the top-level names `test`,
//!   `example`, `invalid` and `localhost` (so `localhost` itself too);
//! - no domain at all, an empty local part, or a domain with no dot;
//! - a domain literal such as `user@[192.0.2.1]`.
//!
//! The comparison ignores case and one trailing dot (`Example.COM.`). Nothing
//! else is judged: a misspelt but real-looking domain passes.
//!
//! The check runs in one place on the way out, [`crate::mail::Mail`]: the
//! site's `deliver` asks before it counts a message against the day's
//! allowance, and `Mail::send` asks again before the transport is called.
//! Stock BOINC checks only syntax (`is_valid_email_syntax` in
//! `html/inc/email.inc`, which is `filter_var` with `FILTER_VALIDATE_EMAIL`)
//! when an address is given, and never at send time; `filter_var` accepts the
//! reserved names and domain literals above and refuses a domain with no dot.
//! The site's sign-up and address change refuse these addresses when the site
//! sends email, with [`CANNOT_RECEIVE`]. Accounts that already have such an
//! address (made by an operator, or before email was on) keep working; they
//! are only sent nothing.

use crate::mail::Mailer;
use crate::site::SiteCtx;

/// Why an address cannot receive mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undeliverable {
    /// A domain reserved for documentation, tests or the local host.
    Reserved,
    /// No `@`, or nothing after it.
    NoDomain,
    /// Nothing before the `@`.
    EmptyLocal,
    /// A domain with no dot (and not one of the reserved names).
    NoDot,
    /// A domain literal in brackets, such as an IP address.
    DomainLiteral,
}

impl Undeliverable {
    /// A fixed word for the log. It never contains any part of the address.
    pub fn as_str(self) -> &'static str {
        match self {
            Undeliverable::Reserved => "reserved-domain",
            Undeliverable::NoDomain => "no-domain",
            Undeliverable::EmptyLocal => "empty-local-part",
            Undeliverable::NoDot => "no-dot-in-domain",
            Undeliverable::DomainLiteral => "domain-literal",
        }
    }
}

/// What came of handing a message to the site's one send path
/// (`site_email::deliver`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The transport accepted it.
    Sent,
    /// Not sent: the address cannot receive mail. Nothing was counted against
    /// the day's allowance and the transport was not called.
    Undeliverable(Undeliverable),
    /// Not sent for another reason: a malformed address, the day's allowance
    /// used, or a transport failure.
    NotSent,
}

impl Delivery {
    pub(crate) fn is_sent(self) -> bool {
        self == Delivery::Sent
    }
}

/// What a page tells someone whose own address is refused. It does not say
/// which rule applied.
pub(crate) const CANNOT_RECEIVE: &str =
    "That email address cannot receive mail, so no message can be sent to it. Please use an \
     address you can read.";

/// Second-level names reserved by RFC 2606: the name and everything under it.
const RESERVED_DOMAINS: [&str; 3] = ["example.com", "example.net", "example.org"];
/// Top-level names reserved by RFC 2606 and RFC 6761.
const RESERVED_TLDS: [&str; 4] = ["test", "example", "invalid", "localhost"];

/// Whether mail to `addr` is certain to bounce, and why. `None` means "not
/// known to be undeliverable", not "valid": the syntax is
/// [`crate::web::is_strict_email`]'s to judge.
pub fn undeliverable(addr: &str) -> Option<Undeliverable> {
    let Some((local, domain)) = addr.rsplit_once('@') else {
        return Some(Undeliverable::NoDomain);
    };
    if local.is_empty() {
        return Some(Undeliverable::EmptyLocal);
    }
    let domain = domain.to_ascii_lowercase();
    let domain = domain.strip_suffix('.').unwrap_or(&domain);
    if domain.is_empty() {
        return Some(Undeliverable::NoDomain);
    }
    if domain.starts_with('[') || domain.ends_with(']') {
        return Some(Undeliverable::DomainLiteral);
    }
    let under = |name: &str| {
        domain == name
            || domain
                .strip_suffix(name)
                .is_some_and(|rest| rest.ends_with('.'))
    };
    let tld = domain.rsplit('.').next().unwrap_or(domain);
    if RESERVED_DOMAINS.iter().any(|d| under(d)) || RESERVED_TLDS.contains(&tld) {
        return Some(Undeliverable::Reserved);
    }
    if !domain.contains('.') {
        return Some(Undeliverable::NoDot);
    }
    None
}

/// Whether the site, which sends email, would refuse to mail `addr`: for the
/// forms that take an address from the person acting on their own account
/// (sign-up, address change). A site that sends no email has nothing to
/// bounce, so it refuses nothing.
pub(crate) fn cannot_receive(ctx: &SiteCtx<'_>, addr: &str) -> bool {
    ctx.mail()
        .is_some_and(|mail| mail.undeliverable(addr).is_some())
}

/// Whether fake mailers in tests keep the reserved names (`@example.org` is
/// what the fixtures use). Only a build with the `test-support` feature, which
/// no deployment enables, has the switch, and only a [`Mailer`] written for
/// tests turns it on; nothing in the project config reaches it.
pub(crate) fn lets_reserved_through(mailer: &dyn Mailer) -> bool {
    #[cfg(feature = "test-support")]
    {
        mailer.allows_reserved_recipients()
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = mailer;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_with_subdomains_in_any_case() {
        for addr in [
            "a@example.com",
            "a@example.net",
            "a@example.org",
            "a@EXAMPLE.ORG",
            "a@mail.example.com",
            "a@a.b.Example.Net",
            "a@example.com.",
            "a@host.test",
            "a@x.example",
            "a@something.invalid",
            "a@box.localhost",
            "a@localhost",
            "a@test",
            "a@example",
            "a@invalid",
            "a@LOCALHOST.",
        ] {
            assert_eq!(undeliverable(addr), Some(Undeliverable::Reserved), "{addr}");
        }
    }

    #[test]
    fn malformed_forms() {
        assert_eq!(undeliverable("nobody"), Some(Undeliverable::NoDomain));
        assert_eq!(undeliverable("a@"), Some(Undeliverable::NoDomain));
        assert_eq!(undeliverable("a@."), Some(Undeliverable::NoDomain));
        assert_eq!(undeliverable(""), Some(Undeliverable::NoDomain));
        assert_eq!(undeliverable("@gmail.com"), Some(Undeliverable::EmptyLocal));
        assert_eq!(undeliverable("@"), Some(Undeliverable::EmptyLocal));
        assert_eq!(undeliverable("a@intranet"), Some(Undeliverable::NoDot));
        assert_eq!(
            undeliverable("a@[192.0.2.1]"),
            Some(Undeliverable::DomainLiteral)
        );
        assert_eq!(
            undeliverable("a@[IPv6:2001:db8::1]"),
            Some(Undeliverable::DomainLiteral)
        );
    }

    #[test]
    fn look_alikes_pass() {
        for addr in [
            "a@example.com.au",
            "a@myexample.com",
            "a@notexample.org",
            "a@example.co",
            "a@test.org",
            "a@testing.com",
            "a@sub.gmail.com",
            "user@sub.gmail.com",
            "a@gmail.com",
            "a@invalid.org",
            "a@localhost.com",
            "a@examples.net",
            "a@exampleorg.com",
        ] {
            assert_eq!(undeliverable(addr), None, "{addr}");
        }
    }

    #[test]
    fn the_log_word_has_no_address_in_it() {
        for r in [
            Undeliverable::Reserved,
            Undeliverable::NoDomain,
            Undeliverable::EmptyLocal,
            Undeliverable::NoDot,
            Undeliverable::DomainLiteral,
        ] {
            assert!(r
                .as_str()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-'));
        }
    }
}
