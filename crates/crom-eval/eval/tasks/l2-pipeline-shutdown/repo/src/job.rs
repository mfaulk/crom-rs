/// One unit of work: the input plus its position in the original input list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub index: usize,
    pub input: String,
}

/// A finished job, reported by the worker that ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub worker: usize,
    pub output: String,
}
