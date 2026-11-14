# buildplan

Plans builds from a tiny manifest format:

```text
# target: dependencies...
app: core net
net: core
core:
```

- `manifest` — parses the manifest.
- `depgraph` — a dependency graph with deterministic topological ordering.
- `planner` — computes the build order and parallel "waves" for a goal.
