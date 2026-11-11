//! Outgoing email: the transport a deployment supplies, and the one place
//! where a message is checked before it reaches the transport.
//!
//! The site builds every message with [`MailMessage::new`], whose fields stay
//! private, so a [`Mailer`] only ever sees addresses that are plain
//! `local@domain` text and single-line subjects and names. Whatever a user
//! typed that ends up in a message (an address, a name) cannot add a header or
//! a recipient.

use async_trait::async_trait;

use crate::web::is_valid_email;

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
}

/// One checked message: one sender, one recipient, a subject and a plain-text
/// body. A transport must encode the sender name and the subject for a header
/// (RFC 5322 quoting or RFC 2047 words) and write the body with CRLF line
/// ends; the strings here have no line breaks, except in the body, which has
/// LF only.
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
    ///   `is_valid_email` form: no space, comma, angle bracket, quote or
    ///   control character. Anything else is refused.
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
        if !is_valid_email(from_addr) {
            return Err(MailError::Invalid("sender address".into()));
        }
        if !is_valid_email(to) {
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
}

/// A mailer with the project's sender settings: what the site sends through.
pub struct Mail<'a> {
    mailer: &'a dyn Mailer,
    from_addr: &'a str,
    from_name: &'a str,
}

impl<'a> Mail<'a> {
    pub(crate) fn new(mailer: &'a dyn Mailer, from_addr: &'a str, from_name: &'a str) -> Self {
        Mail {
            mailer,
            from_addr,
            from_name,
        }
    }

    /// Checks the message (see [`MailMessage::new`]) and hands it to the
    /// transport.
    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), MailError> {
        let message = MailMessage::new(self.from_addr, self.from_name, to, subject, body)?;
        self.mailer.send(&message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
