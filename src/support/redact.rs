//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Keeps credentials out of log lines and error text.

use std::borrow::Cow;

/// Query parameters whose value is a credential.
const SECRET_QUERY_KEYS: [&str; 8] = [
    "password",
    "token",
    "secret",
    "api_key",
    "apikey",
    "access_token",
    "sig",
    "signature",
];

/// Blanks the password in a `scheme://user:password@host` URL and the value of every query
/// parameter in [`SECRET_QUERY_KEYS`].
pub(crate) fn url_password(url: &str) -> Cow<'_, str> {
    let Some(scheme_end) = url.find("://") else {
        return Cow::Borrowed(url);
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |offset| authority_start + offset);
    let authority = &url[authority_start..authority_end];
    let userinfo = authority.rfind('@').and_then(|at| {
        let (user, _password) = authority[..at].split_once(':')?;
        Some(format!("{user}:***{}", &authority[at..]))
    });
    let rest = redact_query(&url[authority_end..]);
    if userinfo.is_none() && matches!(rest, Cow::Borrowed(_)) {
        return Cow::Borrowed(url);
    }
    Cow::Owned(format!(
        "{}{}{rest}",
        &url[..authority_start],
        userinfo.as_deref().unwrap_or(authority)
    ))
}

/// `rest` is a URL from its path on.
fn redact_query(rest: &str) -> Cow<'_, str> {
    let fragment_start = rest.find('#').unwrap_or(rest.len());
    let Some(query_start) = rest[..fragment_start].find('?').map(|at| at + 1) else {
        return Cow::Borrowed(rest);
    };
    let query = &rest[query_start..fragment_start];
    // The name is compared decoded, so `tok%65n` is recognised as `token`.
    let is_secret = |pair: &str| {
        pair.split_once('=').is_some_and(|(key, value)| {
            !value.is_empty()
                && url::form_urlencoded::parse(key.as_bytes())
                    .next()
                    .is_some_and(|(name, _)| {
                        SECRET_QUERY_KEYS.contains(&name.to_ascii_lowercase().as_str())
                    })
        })
    };
    if !query.split('&').any(is_secret) {
        return Cow::Borrowed(rest);
    }
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_secret(pair) => format!("{key}=***"),
            _ => pair.to_string(),
        })
        .collect();
    Cow::Owned(format!(
        "{}{}{}",
        &rest[..query_start],
        redacted.join("&"),
        &rest[fragment_start..]
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
    fn credentials_in_the_query_are_blanked() {
        assert_eq!(
            url_password("https://host/hook?id=7&Token=abc&sig=x%2By#token=frag"),
            "https://host/hook?id=7&Token=***&sig=***#token=frag"
        );
        assert_eq!(
            url_password("redis://app:pw@host?password=pw2&db=1"),
            "redis://app:***@host?password=***&db=1"
        );
        assert_eq!(
            url_password("https://host/hook?tok%65n=abc&API%5Fkey=x&id%3D=7"),
            "https://host/hook?tok%65n=***&API%5Fkey=***&id%3D=7"
        );
    }

    #[test]
    fn a_url_without_a_password_is_untouched() {
        for url in [
            "http://host/x?tokens=3&token=&secret",
            "http://host/x?next=http://a:b@c",
            "tcp://app@host:1883",
            "host:1883",
            "http://host/a:b@c",
        ] {
            assert_eq!(url_password(url), url);
        }
    }
}
