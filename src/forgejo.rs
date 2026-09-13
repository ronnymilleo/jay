//! Minimal Forgejo API client for git-flow PR automation.
//!
//! Configured through environment variables:
//! - `FORGEJO_TOKEN` (required to enable PR automation)
//! - `FORGEJO_URL` (required base URL of your Forgejo server)

use anyhow::{Context, Result};

/// A thin blocking client for the Forgejo REST API (`/api/v1`).
pub struct Forgejo {
    base_url: String,
    token: String,
}

impl Forgejo {
    /// Builds a client from the environment, or `None` when configuration is missing.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("FORGEJO_TOKEN").ok()?;
        let base_url = std::env::var("FORGEJO_URL").ok()?;
        if token.trim().is_empty() || base_url.trim().is_empty() {
            return None;
        }
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Parses a git repo URL into `(owner, repo)`.
    ///
    /// Supports `ssh://user@host[:port]/owner/repo.git`, `git@host:owner/repo.git`
    /// (scp-like) and `http(s)://host/owner/repo.git`.
    pub fn parse_repo(git_repo: &str) -> Option<(String, String)> {
        let s = git_repo.trim();
        let path = if let Some(scheme_end) = s.find("://") {
            let rest = &s[scheme_end + 3..];
            let slash = rest.find('/')?;
            &rest[slash + 1..]
        } else {
            let i = s.rfind(':')?;
            &s[i + 1..]
        };
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            return None;
        }
        let n = parts.len();
        Some((parts[n - 2].to_string(), parts[n - 1].to_string()))
    }

    /// Returns the default branch of a repo (falls back to `"main"`).
    pub fn default_branch(&self, owner: &str, repo: &str) -> Result<String> {
        let url = format!("{}/api/v1/repos/{owner}/{repo}", self.base_url);
        let body: serde_json::Value = self.get(&url).with_context(|| format!("GET {url}"))?;
        Ok(body["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string())
    }

    /// Opens a pull request and returns its HTML URL.
    pub fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        let url = format!("{}/api/v1/repos/{owner}/{repo}/pulls", self.base_url);
        let payload = serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
        });
        let resp = ureq::post(&url)
            .set("Authorization", &format!("token {}", self.token))
            .send_json(payload)
            .with_context(|| format!("POST {url}"))?;
        let pr: serde_json::Value = resp.into_json().context("parse PR response")?;
        let html_url = pr["html_url"].as_str().unwrap_or("").to_string();
        if html_url.is_empty() {
            anyhow::bail!("PR created but no html_url in response: {pr}");
        }
        Ok(html_url)
    }

    /// A GET with the auth header, returning the parsed JSON body.
    fn get(&self, url: &str) -> Result<serde_json::Value> {
        let resp = ureq::get(url)
            .set("Authorization", &format!("token {}", self.token))
            .call()
            .with_context(|| format!("GET {url}"))?;
        resp.into_json().context("parse JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_handles_ssh_and_http_urls() {
        assert_eq!(
            Forgejo::parse_repo("ssh://forgejo@forgejo.example.com:2222/example-owner/jay.git"),
            Some(("example-owner".to_string(), "jay".to_string()))
        );
        assert_eq!(
            Forgejo::parse_repo("git@host:example-owner/jay.git"),
            Some(("example-owner".to_string(), "jay".to_string()))
        );
        assert_eq!(
            Forgejo::parse_repo("http://forgejo.example.com:3000/example-owner/jay.git"),
            Some(("example-owner".to_string(), "jay".to_string()))
        );
        assert_eq!(
            Forgejo::parse_repo("https://forgejo.example.com/example-owner/jay"),
            Some(("example-owner".to_string(), "jay".to_string()))
        );
    }

    #[test]
    fn parse_repo_rejects_non_urls() {
        assert_eq!(Forgejo::parse_repo("/local/path/jay"), None);
        assert_eq!(Forgejo::parse_repo(""), None);
        assert_eq!(Forgejo::parse_repo("justrepo"), None);
    }
}
