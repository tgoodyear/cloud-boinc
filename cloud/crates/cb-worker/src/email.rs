//! The site's [`Mailer`] on Cloudflare Email Service: the `send()` method of a
//! `send_email` binding (see `docs/deploy.md`). What goes into the call is
//! decided by `MailMessage::fields` in `cb-core`; this file only makes the
//! call and turns a failure into a [`MailError`] that is safe to log.

use async_trait::async_trait;
use cb_core::mail::{error_class, MailError, MailMessage, Mailer};
use worker::console_error;
use worker::email::{EmailAddress, SendEmail, SendEmailBuilder};
use worker::js_sys::Reflect;
use worker::wasm_bindgen::JsValue;
use worker::Env;

/// Name of the `send_email` binding. A Worker without it sends no email.
pub const EMAIL: &str = "EMAIL";

/// The email binding, when the Worker has one. Absent (`undefined` in `env`)
/// means the deployment sends no email, and the site behaves as it does with
/// no mailer.
pub fn mailer(env: &Env) -> Option<EmailBinding> {
    env.get_binding::<SendEmail>(EMAIL).ok().map(EmailBinding)
}

pub struct EmailBinding(SendEmail);

#[async_trait(?Send)]
impl Mailer for EmailBinding {
    async fn send(&self, message: &MailMessage) -> Result<(), MailError> {
        let f = message.fields();
        let builder = match f.from_name {
            Some(name) => SendEmailBuilder::builder_with_email_address_and_str(
                &EmailAddress::new(name, f.from_addr),
                f.to,
                f.subject,
            ),
            None => SendEmailBuilder::builder(f.from_addr, f.to, f.subject),
        }
        .text(f.text)
        .build();
        match self.0.send_with_builder(&builder).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let code = Reflect::get(&e, &JsValue::from_str("code"))
                    .ok()
                    .and_then(|v| v.as_string());
                let class = error_class(code.as_deref());
                // The class only: the error's message can name the recipient.
                console_error!("email send failed: {class}");
                Err(MailError::Send(class))
            }
        }
    }
}
