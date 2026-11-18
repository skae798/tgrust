use url::Url;

use crate::model::TextLink;

pub fn normalize(target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty()
        || target
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return None;
    }
    let url = if !target.contains(':')
        || target
            .split_once(':')
            .is_some_and(|(host, _)| host.contains('.'))
    {
        Url::parse(&format!("https://{target}")).ok()?
    } else {
        Url::parse(target).ok()?
    };
    match url.scheme() {
        "http" | "https"
            if url.has_host() && url.username().is_empty() && url.password().is_none() => {}
        "mailto" if url.path().contains('@') => {}
        "tg" if url.has_host() => {}
        _ => return None,
    }
    Some(url.into())
}

pub fn in_text(text: &str) -> Vec<TextLink> {
    let mut links = Vec::new();
    let mut offset = 0;
    for token in text
        .split_inclusive(|ch: char| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '«' | '»'))
    {
        let trimmed = token.trim_end_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '«' | '»')
        });
        let candidate = trimmed.trim_start_matches(['(', '[', '{', '\'']);
        let start = offset + trimmed.len() - candidate.len();
        offset += token.len();
        let lower = candidate.to_ascii_lowercase();
        if !["https://", "http://", "www.", "t.me/", "tg://"]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
        {
            continue;
        }
        let candidate = trim_end(candidate);
        if let Some(uri) = normalize(candidate) {
            links.push(TextLink {
                range: start..start + candidate.len(),
                uri,
            });
        }
    }
    links
}

fn trim_end(mut link: &str) -> &str {
    loop {
        let trimmed = link.trim_end_matches(['.', ',', ';', ':', '?', '!', '\'']);
        if let Some((_, close)) =
            [('(', ')'), ('[', ']'), ('{', '}')]
                .into_iter()
                .find(|&(open, close)| {
                    trimmed.ends_with(close)
                        && trimmed.matches(close).count() > trimmed.matches(open).count()
                })
        {
            link = trimmed.strip_suffix(close).unwrap_or(trimmed);
        } else {
            return trimmed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_links_keep_unicode_offsets_and_balanced_punctuation() {
        let text = "😀 «https://example.org/Тест_(1)» и (t.me/test).";
        let found = in_text(text);
        assert_eq!(
            &text[found[0].range.clone()],
            "https://example.org/Тест_(1)"
        );
        assert_eq!(&text[found[1].range.clone()], "t.me/test");
        assert_eq!(found[1].uri, "https://t.me/test");
    }

    #[test]
    fn external_links_require_safe_schemes() {
        for uri in [
            "file:///tmp/test",
            "javascript:alert(1)",
            "data:text/plain,abc",
            "https://good.org@other.org",
        ] {
            assert!(normalize(uri).is_none());
        }
        assert_eq!(
            normalize("mailto:user@example.org").as_deref(),
            Some("mailto:user@example.org")
        );
    }
}
