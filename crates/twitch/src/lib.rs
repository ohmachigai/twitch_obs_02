/// Twitch API integration placeholder.
///
/// Later milestones will add Helix/EventSub helpers. Keeping a stub function
/// allows integration tests to link against the crate today.
pub fn client_placeholder() {
    // Twitch client initialization will live here.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_placeholder_is_callable() {
        client_placeholder();
    }
}
