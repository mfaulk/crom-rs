use roster::{adult_names, shout_all, Person};

fn person(name: &str, age: u32) -> Person {
    Person {
        name: name.to_string(),
        age,
    }
}

/// Adults (age >= 18) are kept in input order, including the boundary age.
#[test]
fn adult_names_keeps_order_and_boundary() {
    let people = vec![
        person("Zed", 18),
        person("Kid", 17),
        person("Amy", 40),
        person("Bob", 0),
    ];
    assert_eq!(adult_names(&people), vec!["Zed", "Amy"]);
    assert!(adult_names(&[]).is_empty());
}

/// Returned names borrow from the input instead of copying it.
#[test]
fn adult_names_borrows_input() {
    let people = vec![person("Ada", 30)];
    let names = adult_names(&people);
    assert!(std::ptr::eq(names[0].as_ptr(), people[0].name.as_ptr()));
    assert_eq!(people.len(), 1);
}

/// Only tags whose text changes are counted, and all end up uppercase.
#[test]
fn shout_all_counts_changes() {
    let mut tags = vec![
        "a".to_string(),
        "B".to_string(),
        "ß".to_string(),
        "123".to_string(),
        "MiXeD".to_string(),
    ];
    assert_eq!(shout_all(&mut tags), 3);
    assert_eq!(tags, vec!["A", "B", "SS", "123", "MIXED"]);
    let mut none: Vec<String> = Vec::new();
    assert_eq!(shout_all(&mut none), 0);
}
