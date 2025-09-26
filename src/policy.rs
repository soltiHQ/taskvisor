#[derive(Clone, Copy, Debug)]
pub enum RestartPolicy {
    Never,
    Always,
    OnFailure,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}
