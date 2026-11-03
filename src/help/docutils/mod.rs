//! Placeholder until the docutils port lands.

pub(crate) struct Published {
    pub man: String,
    pub stderr: String,
}

pub(crate) fn publish_man(_rst: &str) -> Result<Published, String> {
    Err("docutils is not ported yet".to_string())
}
