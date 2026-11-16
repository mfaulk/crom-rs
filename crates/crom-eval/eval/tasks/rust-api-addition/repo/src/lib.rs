/// A last-in, first-out stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack<T> {
    items: Vec<T>,
}

impl<T> Stack<T> {
    /// An empty stack.
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Push `item` onto the top of the stack.
    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// Remove and return the top item, or `None` if the stack is empty.
    pub fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }

    /// Number of items on the stack.
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_is_lifo() {
        let mut s = Stack::new();
        s.push(1);
        s.push(2);
        assert_eq!(s.len(), 2);
        assert_eq!(s.pop(), Some(2));
        assert_eq!(s.pop(), Some(1));
        assert_eq!(s.pop(), None);
    }
}
