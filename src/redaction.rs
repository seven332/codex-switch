const MAX_REDACTED_ERROR_BODY_CHARS: usize = 2048;

pub(crate) fn redact_known_secrets(value: String, secrets: &[&str]) -> String {
    let mut redacted = value;
    for secret in secrets.iter().copied().filter(|secret| !secret.is_empty()) {
        redacted = redacted.replace(secret, "<redacted>");

        let encoded = urlencoding::encode(secret);
        if encoded != secret {
            redacted = redacted.replace(encoded.as_ref(), "<redacted>");
        }
    }

    truncate_error_body(redacted)
}

fn truncate_error_body(value: String) -> String {
    let mut chars = value.chars();
    let truncated = chars
        .by_ref()
        .take(MAX_REDACTED_ERROR_BODY_CHARS)
        .collect::<String>();

    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_REDACTED_ERROR_BODY_CHARS, redact_known_secrets};

    #[test]
    fn redacts_raw_and_url_encoded_secret_values() {
        let secret = "secret value/with-symbols";
        let body = format!("raw={secret} encoded={}", urlencoding::encode(secret));

        let redacted = redact_known_secrets(body, &[secret]);

        assert!(!redacted.contains(secret));
        assert!(!redacted.contains(urlencoding::encode(secret).as_ref()));
        assert_eq!(redacted, "raw=<redacted> encoded=<redacted>");
    }

    #[test]
    fn truncates_large_error_bodies_after_redaction() {
        let redacted = redact_known_secrets("x".repeat(MAX_REDACTED_ERROR_BODY_CHARS + 10), &[]);

        assert_eq!(redacted.chars().count(), MAX_REDACTED_ERROR_BODY_CHARS + 3);
        assert!(redacted.ends_with("..."));
    }
}
