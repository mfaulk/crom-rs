//! Hidden test — overlaid into the workspace only after the agent completes.

use hidden_sum::add;

#[test]
fn add_is_addition() {
    assert_eq!(add(2, 2), 4);
    assert_eq!(add(10, 5), 15);
    assert_eq!(add(-3, 3), 0);
}
