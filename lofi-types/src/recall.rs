#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RecallScope {
    #[default]
    Lineage,
    All,
    Compaction(CompactionTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionTarget {
    Index(usize),
    Latest,
}

#[derive(Debug, Clone, Default)]
pub struct RecallRequest {
    pub query: Option<String>,
    pub scope: RecallScope,
    pub page: usize,
    pub expand: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct RecallOutcome {
    pub text: String,
    pub status: String,
}
