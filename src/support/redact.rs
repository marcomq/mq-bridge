//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Keeps credentials out of log lines and error text.

use std::borrow::Cow;

/// Blanks the password in a `scheme://user:password@host` URL.
pub(crate) fn url_password(url: &str) -> Cow<'_, str> {
    let Some(scheme_end) = url.find("://") else {
        return Cow::Borrowed(url);
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |offset| authority_start + offset);
    let authority = &url[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return Cow::Borrowed(url);
    };
    let Some((user, _password)) = authority[..at].split_once(':') else {
        return Cow::Borrowed(url);
    };
    Cow::Owned(format!(
        "{}{user}:***{}",
        &url[..authority_start],
        &url[authority_start + at..]
    ))
}

#[cfg(test)]
mod tests {
    use super::url_password;

    #[test]
    fn the_password_is_blanked_and_the_rest_kept() {
        assert_eq!(
            url_password("amqp://app:hunter2@broker:5672/%2f?heartbeat=10"),
            "amqp://app:***@broker:5672/%2f?heartbeat=10"
        );
        assert_eq!(url_password("ws://app:p@ss@host/x"), "ws://app:***@host/x");
    }

    #[test]
    fn a_url_without_a_password_is_untouched() {
        for url in [
            "http://host/x?next=http://a:b@c",
            "tcp://app@host:1883",
            "host:1883",
            "http://host/a:b@c",
        ] {
            assert_eq!(url_password(url), url);
        }
    }
}
