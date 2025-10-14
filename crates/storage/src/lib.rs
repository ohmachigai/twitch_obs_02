/// Storage layer placeholder.
///
/// Future PRs will add database accessors backed by SQLite. For now we only
/// guarantee the crate links correctly by exposing a no-op initializer.
pub fn initialize_storage() {
    // Database connections will be managed here in later milestones.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_initializer_is_idempotent() {
        initialize_storage();
        initialize_storage();
    }
}
