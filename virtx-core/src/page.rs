//! A page of a list by name, as `snapshot.list` and `volume.list` take one.

/// What is named after `after` in `items`, in order by name, `limit` at most: every one
/// without either.
pub fn page<T>(
    items: impl IntoIterator<Item = T>,
    name: impl Fn(&T) -> &str,
    after: Option<&str>,
    limit: Option<u32>,
) -> Vec<T> {
    let mut items: Vec<T> = items
        .into_iter()
        .filter(|item| after.is_none_or(|after| name(item) > after))
        .collect();
    items.sort_by(|a, b| name(a).cmp(name(b)));
    if let Some(limit) = limit {
        items.truncate(limit as usize);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_what_is_named_after_its_start_in_order() {
        let names = ["c", "a", "d", "b"];
        let all = page(names, |n| n, None, None);
        assert_eq!(all, ["a", "b", "c", "d"]);
        assert_eq!(page(names, |n| n, None, Some(2)), ["a", "b"]);
        assert_eq!(page(names, |n| n, Some("b"), Some(2)), ["c", "d"]);
        assert_eq!(page(names, |n| n, Some("d"), Some(2)), [] as [&str; 0]);
    }
}
