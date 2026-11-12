//! The site's [`Mailer`] on Cloudflare Email Service: the `send()` method of a
//! `send_email` binding (see `docs/deploy.md`). What goes into the call is
//! decided by `MailMessage::fields` in `cb-core`; this file only makes the
//! call and turns a failure into a [`MailError`] that is safe to log.

use std::cell::Cell;

use async_trait::async_trait;
use cb_core::mail::{error_class, MailError, MailMessage, Mailer};
use worker::console_error;
use worker::email::{EmailAddress, SendEmail, SendEmailBuilder};
use worker::js_sys::Reflect;
use worker::wasm_bindgen::JsValue;
use worker::Env;

/// Name of the `send_email` binding. A Worker without it sends no email.
pub const EMAIL: &str = "EMAIL";

thread_local! {
    static WARNED: Cell<bool> = const { Cell::new(false) };
}

/// The email binding, when the Worker has one. Absent (`undefined` in `env`)
/// means the deployment sends no email, and the site behaves as it does with
/// no mailer; that is silent. A value that is there but has no `send`
/// function is not a send binding: the site then sends no email either, and
/// one line says so, once for each isolate.
pub fn mailer(env: &Env) -> Option<EmailBinding> {
    let value = Reflect::get(env, &JsValue::from_str(EMAIL)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    let usable = Reflect::get(&value, &JsValue::from_str("send"))
        .map(|f| f.is_function())
        .unwrap_or(false);
    match env.get_binding::<SendEmail>(EMAIL) {
        Ok(binding) if usable => Some(EmailBinding(binding)),
        _ => {
            if !WARNED.with(|w| w.replace(true)) {
                console_error!(
                    "email: binding {EMAIL} is not a send_email binding; sending no email"
                );
            }
            None
        }
    }
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
