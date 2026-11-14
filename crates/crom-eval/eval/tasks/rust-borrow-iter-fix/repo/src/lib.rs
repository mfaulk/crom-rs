/// A member of a roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub age: u32,
}

/// Names of people aged 18 or over, in input order, borrowed from `people`.
pub fn adult_names(people: &[Person]) -> Vec<&str> {
    people
        .iter()
        .filter(|p| p.age >= 18)
        .map(|p| p.name)
        .collect()
}

/// Uppercase every tag in place; return how many tags actually changed.
pub fn shout_all(tags: &mut Vec<String>) -> usize {
    let mut changed = 0;
    for tag in tags.iter() {
        let upper = tag.to_uppercase();
        if upper != *tag {
            *tag = upper;
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_adults() {
        let people = vec![
            Person {
                name: "Ada".into(),
                age: 36,
            },
            Person {
                name: "Tim".into(),
                age: 12,
            },
        ];
        assert_eq!(adult_names(&people), vec!["Ada"]);
    }

    #[test]
    fn shouts_tags() {
        let mut tags = vec!["rust".to_string(), "OK".to_string()];
        assert_eq!(shout_all(&mut tags), 1);
        assert_eq!(tags, vec!["RUST", "OK"]);
    }
}
