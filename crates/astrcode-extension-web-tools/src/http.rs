use std::time::Duration;

use reqwest::{Client, redirect::Policy};

pub(crate) fn build_client(timeout_secs: u64, user_agent: &str) -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .user_agent(user_agent)
        .redirect(Policy::limited(5))
        .build()
}

pub(crate) fn build_fetch_client(
    timeout_secs: u64,
    user_agent: &str,
) -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .user_agent(user_agent)
        .redirect(Policy::none())
        .build()
}
