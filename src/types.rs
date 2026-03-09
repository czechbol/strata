use git2::Oid;
use serde::Serialize;

#[derive(Serialize)]
pub struct OutputData {
    pub repo: String,
    pub granularity: String,
    pub generated_at: String,
    pub head_commit: String,
    pub periods: Vec<String>,
    pub authors: Vec<String>,
    pub series: Vec<SeriesPoint>,
    pub tags: Vec<Tag>,
}

#[derive(Serialize)]
pub struct SeriesPoint {
    pub ts: i64,
    pub total: u64,
    pub counts: Vec<(usize, u64)>,
    pub author_counts: Vec<(usize, u64)>,
    pub summary: String,
    pub author: String,
}

#[derive(Serialize)]
pub struct Tag {
    pub name: String,
    pub ts: i64,
    pub importance: f64,
}

pub struct WorkItem {
    pub commit_oid: Oid,
    pub blob_oid: Oid,
}
