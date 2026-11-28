pub fn all() -> impl Iterator<Item = (&'static str, &'static str)> {
    include_str!("../data/poll-countries.tsv")
        .lines()
        .filter_map(|line| line.split_once('\t'))
}

pub fn name(code: &str) -> Option<&'static str> {
    all().find_map(|(key, name)| (key == code).then_some(name))
}

pub fn normalize(codes: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut codes: Vec<_> = codes
        .into_iter()
        .map(|code| code.trim().to_ascii_uppercase())
        .collect();
    anyhow::ensure!(
        codes.iter().all(|code| name(code).is_some()),
        "Выберите страну из списка"
    );
    codes.sort_unstable();
    codes.dedup();
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn country_selection_is_normalized_without_guessing_unknown_codes() {
        assert_eq!(all().count(), 249);
        assert_eq!(
            normalize(vec![" ua ".into(), "PL".into(), "UA".into()]).unwrap(),
            ["PL", "UA"]
        );
        assert!(normalize(Vec::new()).unwrap().is_empty());
        for code in ["", "USA", "XX", "UА", "🇺🇦"] {
            assert!(normalize(vec![code.into()]).is_err());
        }
    }
}
