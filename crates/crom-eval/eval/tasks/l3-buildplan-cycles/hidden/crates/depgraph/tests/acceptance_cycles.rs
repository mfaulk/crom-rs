use depgraph::{Graph, GraphError};

fn g(nodes: &[&str], edges: &[(&str, &str)]) -> Graph {
    let mut g = Graph::new();
    for n in nodes {
        g.add_node(n);
    }
    for (a, b) in edges {
        g.add_dep(a, b).unwrap();
    }
    g
}

#[test]
fn reports_cycle_from_smallest_node() {
    let graph = g(
        &["m", "b", "q", "x", "leaf"],
        &[
            ("m", "q"),
            ("q", "b"),
            ("b", "m"),
            ("x", "m"),
            ("q", "leaf"),
        ],
    );
    assert_eq!(
        graph.topo_order(),
        Err(GraphError::Cycle(vec!["b".into(), "m".into(), "q".into()]))
    );
}

#[test]
fn self_dependency_is_a_cycle() {
    let graph = g(&["a", "z"], &[("a", "a"), ("z", "a")]);
    assert_eq!(graph.topo_order(), Err(GraphError::Cycle(vec!["a".into()])));
}

#[test]
fn cycle_display() {
    let e = GraphError::Cycle(vec!["b".into(), "m".into(), "q".into()]);
    assert_eq!(e.to_string(), "dependency cycle: b -> m -> q -> b");
    assert_eq!(
        GraphError::Cycle(vec!["a".into()]).to_string(),
        "dependency cycle: a -> a"
    );
}

#[test]
fn acyclic_order_unchanged() {
    let graph = g(
        &["d", "c", "b", "a"],
        &[("d", "b"), ("d", "c"), ("b", "a"), ("c", "a")],
    );
    assert_eq!(graph.topo_order().unwrap(), ["a", "b", "c", "d"]);
}
