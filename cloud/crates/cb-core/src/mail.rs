//! Outgoing email: the transport a deployment supplies, and the one place
//! where a message is checked before it reaches the transport.
//!
//! The site builds every message with [`MailMessage::new`], whose fields stay
//! private, so a [`Mailer`] only ever sees addresses that are plain
//! `local@domain` text and single-line subjects and names. Whatever a user
//! typed that ends up in a message (an address, a name) cannot add a header or
//! a recipient.

use async_trait::async_trait;

use crate::mail_guard::{lets_reserved_through, undeliverable, Undeliverable};
use crate::web::is_strict_email;

/// Longest subject sent, in characters.
const MAX_SUBJECT_CHARS: usize = 200;
/// Longest body accepted, in bytes. The site's messages are a few hundred.
const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MailError {
    /// The message was refused before sending.
    #[error("mail: {0}")]
    Invalid(String),
    /// The transport could not send it.
    #[error("mail: {0}")]
    Send(String),
    /// The recipient cannot receive mail (see [`crate::mail_guard`]); the
    /// transport was not called. The text names the rule, not the address.
    #[error("mail: undeliverable address ({0})")]
    Undeliverable(&'static str),
}

/// One checked message: one sender, one recipient, a subject and a plain-text
/// body. The strings here have no line breaks, except in the body, which has
/// LF only. A transport that writes the MIME message itself must encode the
/// sender name and the subject for a header (RFC 5322 quoting or RFC 2047
/// words) and write the body with CRLF line ends; one that hands the fields
/// to a service that builds the message (see [`MailMessage::fields`]) leaves
/// that to the service.
#[derive(Debug, Clone, PartialEq)]
pub struct MailMessage {
    from_addr: String,
    from_name: String,
    to: String,
    subject: String,
    body: String,
}

impl MailMessage {
    /// Checks and cleans the parts of a message.
    ///
    /// - `from_addr` and `to` must be a single address of the site's
    ///   `is_strict_email` form: no space, comma, angle bracket, quote,
    ///   `%` or control character, and a well-formed domain. Anything else
    ///   is refused.
    /// - `from_name` and `subject` lose control characters (CR and LF
    ///   included) and line and paragraph separators, which become a space;
    ///   `from_name` also loses the characters that delimit a mailbox in a
    ///   header (`<>"\,;:@()[]`).
    /// - `body` loses CR and every control character except LF and tab.
    pub fn new(
        from_addr: &str,
        from_name: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<MailMessage, MailError> {
        if !is_strict_email(from_addr) {
            return Err(MailError::Invalid("sender address".into()));
        }
        if !is_strict_email(to) {
            return Err(MailError::Invalid("recipient address".into()));
        }
        let body: String = body
            .chars()
            .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
            .collect();
        if body.len() > MAX_BODY_BYTES {
            return Err(MailError::Invalid("body too long".into()));
        }
        let name: String = one_line(from_name)
            .chars()
            .filter(|c| !"<>\"\\,;:@()[]".contains(*c))
            .collect();
        Ok(MailMessage {
            from_addr: from_addr.to_string(),
            from_name: name.trim().to_string(),
            to: to.to_string(),
            subject: one_line(subject).chars().take(MAX_SUBJECT_CHARS).collect(),
            body,
        })
    }

    pub fn from_addr(&self) -> &str {
        &self.from_addr
    }

    /// May be empty.
    pub fn from_name(&self) -> &str {
        &self.from_name
    }

    /// The only recipient.
    pub fn to(&self) -> &str {
        &self.to
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    /// The parts of the message as the structured `send()` call of Cloudflare
    /// Email Service takes them: `from` as an address with an optional name,
    /// one `to`, `subject` and `text`. The name is `None` when empty.
    pub fn fields(&self) -> SendFields<'_> {
        SendFields {
            from_addr: &self.from_addr,
            from_name: (!self.from_name.is_empty()).then_some(self.from_name.as_str()),
            to: &self.to,
            subject: &self.subject,
            text: &self.body,
        }
    }
}

/// See [`MailMessage::fields`].
#[derive(Debug, Clone, PartialEq)]
pub struct SendFields<'a> {
    pub from_addr: &'a str,
    pub from_name: Option<&'a str>,
    pub to: &'a str,
    pub subject: &'a str,
    pub text: &'a str,
}

/// What a transport may log of a failed send: the `code` of the error the
/// service threw (`E_SENDER_NOT_VERIFIED`, `E_RATE_LIMIT_EXCEEDED`, ...) if it
/// has the form of one, and `unknown` otherwise. Never the error's message,
/// which could repeat an address.
pub fn error_class(code: Option<&str>) -> String {
    match code {
        Some(c)
            if c.starts_with("E_")
                && c.len() <= 64
                && c.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') =>
        {
            c.to_string()
        }
        _ => "unknown".to_string(),
    }
}

/// `s` on one line: each run of control characters and line separators
/// becomes one space, and the ends are trimmed.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out.trim().to_string()
}

/// How a deployment sends email. The site calls it from a request, so
/// `send` should return when the transport has accepted the message, not wait
/// for delivery.
#[async_trait(?Send)]
pub trait Mailer {
    async fn send(&self, message: &MailMessage) -> Result<(), MailError>;

    /// Tells the log that a message was not handed to the transport because
    /// its recipient cannot receive mail. `reason` is a fixed word
    /// ([`Undeliverable::as_str`]); neither the address nor anything of it is
    /// passed. The default logs nothing.
    fn log_refused(&self, _userid: i64, _reason: &str) {}

    /// For fake transports in tests only, and only in a build with the
    /// `test-support` feature: whether the reserved example names
    /// (`@example.org`) go through to this transport. No deployment build has
    /// this method, and no setting reaches it.
    #[cfg(feature = "test-support")]
    fn allows_reserved_recipients(&self) -> bool {
        false
    }
}

/// A mailer with the project's sender settings: what the site sends through.
pub struct Mail<'a> {
    mailer: &'a dyn Mailer,
    from_addr: &'a str,
    from_name: &'a str,
    reserved_ok: bool,
}

impl<'a> Mail<'a> {
    pub(crate) fn new(mailer: &'a dyn Mailer, from_addr: &'a str, from_name: &'a str) -> Self {
        Mail {
            mailer,
            from_addr,
            from_name,
            reserved_ok: lets_reserved_through(mailer),
        }
    }

    /// Why mail to `to` would certain bounce, if it would (see
    /// [`crate::mail_guard`]).
    pub(crate) fn undeliverable(&self, to: &str) -> Option<Undeliverable> {
        undeliverable(to).filter(|r| !(self.reserved_ok && *r == Undeliverable::Reserved))
    }

    /// [`Mail::undeliverable`] for the address of account `userid`, and when
    /// it is refused, a line in the log with the reason and the account (never
    /// the address). Every place that skips a message to such an address asks
    /// here.
    pub(crate) fn refuse(&self, userid: i64, to: &str) -> Option<Undeliverable> {
        let reason = self.undeliverable(to)?;
        self.mailer.log_refused(userid, reason.as_str());
        Some(reason)
    }

    /// Checks the message (see [`MailMessage::new`]) and hands it to the
    /// transport, unless the recipient cannot receive mail.
    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), MailError> {
        if let Some(reason) = self.undeliverable(to) {
            return Err(MailError::Undeliverable(reason.as_str()));
        }
        let message = MailMessage::new(self.from_addr, self.from_name, to, subject, body)?;
        self.mailer.send(&message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A transport that counts its calls; `open` lets reserved names through.
    struct Counting {
        calls: Cell<u32>,
        open: bool,
    }

    #[async_trait(?Send)]
    impl Mailer for Counting {
        async fn send(&self, _: &MailMessage) -> Result<(), MailError> {
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }

        fn allows_reserved_recipients(&self) -> bool {
            self.open
        }
    }

    #[test]
    fn send_refuses_what_cannot_receive_mail_before_the_transport() {
        let run = |open: bool, to: &str| {
            let t = Counting {
                calls: Cell::new(0),
                open,
            };
            let mail = Mail::new(&t, "boinc@p.example", "P");
            let out = futures_lite::future::block_on(mail.send(to, "s", "b"));
            (out, t.calls.get())
        };
        assert_eq!(
            run(false, "a@example.org"),
            (Err(MailError::Undeliverable("reserved-domain")), 0)
        );
        assert_eq!(run(false, "a@sub.example.com").1, 0);
        assert_eq!(run(false, "a@example.com.au"), (Ok(()), 1));
        // A test transport that opts in gets the reserved names, nothing else.
        assert_eq!(run(true, "a@example.org"), (Ok(()), 1));
        assert!(run(true, "a@x.invalid").0.is_ok());
    }

    #[test]
    fn line_breaks_cannot_leave_the_subject_or_name() {
        let m = MailMessage::new(
            "boinc@example.org",
            "Proj\r\nBcc: x@example.org",
            "a@example.org",
            "Hi\r\nBcc: x@example.org\u{2028}Cc: y@example.org",
            "one\r\ntwo\0three\tfour",
        )
        .unwrap();
        assert_eq!(m.from_name(), "Proj Bcc xexample.org");
        assert_eq!(m.subject(), "Hi Bcc: x@example.org Cc: y@example.org");
        assert!(!m.subject().contains(['\r', '\n']));
        assert_eq!(m.body(), "one\ntwothree\tfour");
    }

    #[test]
    fn the_send_fields_are_the_checked_parts() {
        let m =
            MailMessage::new("boinc@example.org", "Proj", "a@example.org", "Hi", "text\n").unwrap();
        assert_eq!(
            m.fields(),
            SendFields {
                from_addr: "boinc@example.org",
                from_name: Some("Proj"),
                to: "a@example.org",
                subject: "Hi",
                text: "text\n",
            }
        );
        let bare =
            MailMessage::new("boinc@example.org", " \r\n", "a@example.org", "s", "b").unwrap();
        assert_eq!(bare.fields().from_name, None);
    }

    #[test]
    fn only_an_error_code_is_logged() {
        assert_eq!(
            error_class(Some("E_RATE_LIMIT_EXCEEDED")),
            "E_RATE_LIMIT_EXCEEDED"
        );
        for bad in [
            None,
            Some(""),
            Some("ada@example.org"),
            Some("E_x"),
            Some("E_SENDER bad"),
            Some(&"E_A".repeat(40)[..]),
        ] {
            assert_eq!(error_class(bad), "unknown", "{bad:?}");
        }
    }

    #[test]
    fn a_recipient_is_a_well_formed_address() {
        let long_local = format!("{}@example.org", "a".repeat(65));
        let long_label = format!("a@{}.org", "b".repeat(64));
        let long_all = format!(
            "a@{}.{}.{}.{}.org",
            "c".repeat(63),
            "d".repeat(63),
            "e".repeat(63),
            "f".repeat(63)
        );
        assert!(is_strict_email("a.b+c-d@example.org"));
        assert!(is_strict_email(&format!("{}@example.org", "a".repeat(64))));
        assert!(is_strict_email(&format!("a@{}.org", "b".repeat(63))));
        for bad in [
            "a%b@example.org",
            "-a@example.org",
            ".a@example.org",
            "a.@example.org",
            "a..b@example.org",
            "a@.example.org",
            "a@example..org",
            "a@example.org.",
            "a@-example.org",
            "a@example-.org",
            "a@example.-org",
            "a@example",
            "a@127.0.0.1",
            "a@1.2",
            "a@example.123",
            "@example.org",
            "a@b@example.org",
            long_local.as_str(),
            long_label.as_str(),
            long_all.as_str(),
        ] {
            assert!(!is_strict_email(bad), "{bad:?}");
            assert!(
                MailMessage::new("boinc@example.org", "", bad, "s", "b").is_err(),
                "{bad:?}"
            );
        }
        // The sender is held to the same rule.
        assert!(MailMessage::new("a%b@example.org", "", "a@example.org", "s", "b").is_err());
    }

    #[test]
    fn a_recipient_is_one_plain_address() {
        for bad in [
            "a@example.org\r\nBcc: x@example.org",
            "a@example.org,x@example.org",
            "a@example.org x@example.org",
            "<a@example.org>",
            "\"a\"@example.org",
            "a@example.org\n",
            "",
        ] {
            assert!(
                MailMessage::new("boinc@example.org", "", bad, "s", "b").is_err(),
                "{bad:?}"
            );
            assert!(
                MailMessage::new(bad, "", "a@example.org", "s", "b").is_err(),
                "{bad:?}"
            );
        }
    }
}
